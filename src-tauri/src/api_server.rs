use crate::browser::ProxySettings;
use crate::events;
use crate::group_manager::GROUP_MANAGER;
use crate::profile::manager::ProfileManager;
use crate::proxy_manager::PROXY_MANAGER;
use crate::tag_manager::TAG_MANAGER;
use axum::{
  extract::{Path, Query, State},
  http::{header, HeaderMap, Method, StatusCode},
  middleware::{self, Next},
  response::{IntoResponse, Json, Response},
  routing::get,
  Router,
};
use futures_util::stream::{self, StreamExt};
use lazy_static::lazy_static;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, Mutex};
use tower_http::cors::CorsLayer;
use utoipa::{OpenApi, ToSchema};
use utoipa_axum::{router::OpenApiRouter, routes};

// API Types

/// How many profile stops run concurrently inside one batch request.
/// Launches use the global launch scheduler instead (see
/// `batch_run_profiles`), which is the single concurrency controller.
const BATCH_RUN_CONCURRENCY: usize = 8;

#[derive(Debug, Serialize, Deserialize, Clone, ToSchema)]
pub struct ApiProfile {
  pub id: String,
  pub name: String,
  pub browser: String,
  pub version: String,
  pub proxy_id: Option<String>,
  pub launch_hook: Option<String>,
  pub process_id: Option<u32>,
  pub last_launch: Option<u64>,
  pub release_type: String,
  #[schema(value_type = Object)]
  pub group_id: Option<String>,
  pub tags: Vec<String>,
  pub is_running: bool,
  pub proxy_bypass_rules: Vec<String>,
  pub vpn_id: Option<String>,
  pub clear_on_close: bool,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct ApiProfilesResponse {
  pub profiles: Vec<ApiProfile>,
  pub total: usize,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct ApiProfileResponse {
  pub profile: ApiProfile,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct CreateProfileRequest {
  pub name: String,
  /// Browser engine. Must be `"chromium"` (anti-detect Chromium). Any other
  /// value (e.g. `"chromium"`) is rejected with 400.
  pub browser: String,
  /// Optional. Omit (or pass `"latest"`) to use the newest already-downloaded
  /// version of the chosen browser. A concrete version must already be
  /// downloaded; the create path does not fetch new versions.
  #[serde(default)]
  pub version: Option<String>,
  pub proxy_id: Option<String>,
  pub vpn_id: Option<String>,
  pub launch_hook: Option<String>,
  pub release_type: Option<String>,
  /// Browser fingerprint/config. Send only when `browser` is `"chromium"`.
  /// Omit it, or pass an empty object `{}`, to have a fresh fingerprint
  /// generated automatically at creation. Provide a `fingerprint` field to
  /// pin a specific one.
  #[schema(value_type = Option<Object>)]
  pub chromium_config: Option<serde_json::Value>,
  pub group_id: Option<String>,
  pub tags: Option<Vec<String>>,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct UpdateProfileRequest {
  pub name: Option<String>,
  // No `browser` field: a profile's engine is fixed at creation (changing it
  // would invalidate the generated fingerprint and on-disk profile dir).
  // Accepting it here only to silently ignore it misled API clients.
  pub version: Option<String>,
  pub proxy_id: Option<String>,
  pub vpn_id: Option<String>,
  pub launch_hook: Option<String>,
  pub release_type: Option<String>,
  pub group_id: Option<String>,
  pub tags: Option<Vec<String>>,
  pub extension_group_id: Option<String>,
  pub proxy_bypass_rules: Option<Vec<String>>,
  /// One of "Disabled", "Regular", "Encrypted".
  pub sync_mode: Option<String>,
  /// Wipe browsing data (keeping extensions and bookmarks) when the browser
  /// exits. Rejected (400) for ephemeral or password-protected profiles.
  pub clear_on_close: Option<bool>,
}

#[derive(Clone)]
struct ApiServerState {
  app_handle: tauri::AppHandle,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
struct ApiGroupResponse {
  id: String,
  name: String,
  profile_count: usize,
}

#[derive(Debug, Deserialize, ToSchema)]
struct CreateGroupRequest {
  name: String,
}

#[derive(Debug, Deserialize, ToSchema)]
struct UpdateGroupRequest {
  name: String,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
struct ApiProxyResponse {
  id: String,
  name: String,
  #[schema(value_type = Object)]
  proxy_settings: ProxySettings,
}

#[derive(Debug, Deserialize, ToSchema)]
struct CreateProxyRequest {
  name: String,
  #[schema(value_type = Object)]
  proxy_settings: ProxySettings,
}

#[derive(Debug, Deserialize, ToSchema)]
struct UpdateProxyRequest {
  name: Option<String>,
  #[schema(value_type = Option<Object>)]
  proxy_settings: Option<ProxySettings>,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
struct ApiVpnResponse {
  id: String,
  name: String,
  /// "WireGuard" or "Vless"
  vpn_type: String,
  created_at: i64,
  last_used: Option<i64>,
}

#[derive(Debug, Serialize, ToSchema)]
struct ApiVpnExportResponse {
  id: String,
  name: String,
  /// "WireGuard" or "Vless"
  vpn_type: String,
  /// Raw config content (decrypted): WireGuard `.conf` or VLESS share link/JSON
  config_data: String,
}

#[derive(Debug, Deserialize, ToSchema)]
struct ImportVpnRequest {
  /// Raw config content: WireGuard `.conf`, VLESS `vless://` link, or Xray JSON
  content: String,
  /// Original filename
  filename: String,
  /// Optional display name; defaults to filename-based name
  name: Option<String>,
}

#[derive(Debug, Deserialize, ToSchema)]
struct CreateVpnRequest {
  name: String,
  /// "WireGuard" or "Vless"
  vpn_type: String,
  config_data: String,
}

#[derive(Debug, Deserialize, ToSchema)]
struct UpdateVpnRequest {
  name: String,
}

#[derive(Debug, Deserialize, ToSchema)]
struct DownloadBrowserRequest {
  browser: String,
  version: String,
}

#[derive(Debug, Serialize, ToSchema)]
struct DownloadBrowserResponse {
  browser: String,
  version: String,
  status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ToastPayload {
  pub message: String,
  pub variant: String,
  pub title: String,
  pub description: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
struct RunProfileResponse {
  profile_id: String,
  remote_debugging_port: u16,
  headless: bool,
}

#[derive(Debug, Deserialize, ToSchema)]
struct RunProfileRequest {
  url: Option<String>,
  headless: Option<bool>,
}

#[derive(Debug, Deserialize, ToSchema)]
struct OpenUrlRequest {
  url: String,
}

#[derive(Debug, Deserialize, ToSchema)]
struct ImportCookiesRequest {
  /// Raw cookie file content. Format is auto-detected: a JSON array
  /// (Puppeteer / EditThisCookie style) or a Netscape `cookies.txt`.
  content: String,
}

#[derive(Debug, Serialize, ToSchema)]
struct ImportCookiesResponse {
  cookies_imported: usize,
  cookies_replaced: usize,
  errors: Vec<String>,
}

#[derive(Debug, Deserialize, ToSchema)]
struct BatchRunRequest {
  /// Profile IDs to launch.
  profile_ids: Vec<String>,
  /// Optional URL to open in every launched profile.
  url: Option<String>,
  /// Launch headless. Defaults to false.
  headless: Option<bool>,
}

#[derive(Debug, Serialize, ToSchema)]
struct BatchRunResult {
  profile_id: String,
  /// Whether this profile launched successfully.
  ok: bool,
  /// Remote debugging port if launched, otherwise null.
  remote_debugging_port: Option<u16>,
  /// Failure reason if not launched, otherwise null.
  error: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
struct BatchRunResponse {
  results: Vec<BatchRunResult>,
}

#[derive(Debug, Deserialize, ToSchema)]
struct BatchStopRequest {
  /// Profile IDs to stop.
  profile_ids: Vec<String>,
}

#[derive(Debug, Serialize, ToSchema)]
struct BatchStopResult {
  profile_id: String,
  /// Whether this profile was stopped successfully.
  ok: bool,
  /// Failure reason if not stopped, otherwise null.
  error: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
struct BatchStopResponse {
  results: Vec<BatchStopResult>,
}

#[derive(Debug, Deserialize, ToSchema)]
struct CreateProxyPoolRequest {
  /// Pool name.
  name: String,
  /// Stored proxy IDs that make up the pool.
  proxy_ids: Vec<String>,
}

#[derive(Debug, Serialize, ToSchema)]
struct ApiProxyPoolResponse {
  id: String,
  name: String,
  proxy_ids: Vec<String>,
  #[serde(default)]
  updated_at: Option<u64>,
}

#[derive(Debug, Deserialize, ToSchema)]
struct UpdateProxyPoolRequest {
  /// New pool name.
  name: String,
  /// New member proxy IDs (order is significant for round-robin).
  proxy_ids: Vec<String>,
}

#[derive(Debug, Deserialize, ToSchema)]
struct AssignProfilesToPoolRequest {
  /// Pool to distribute proxies from.
  pool_id: String,
  /// Profiles to assign a pool member to (round-robin).
  profile_ids: Vec<String>,
}

#[derive(Debug, Serialize, ToSchema)]
struct PoolAssignItem {
  profile_id: String,
  /// Whether the profile was assigned successfully.
  ok: bool,
  /// Assigned proxy ID when successful.
  proxy_id: Option<String>,
  /// Failure reason when not assigned.
  error: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
struct AssignProfilesToPoolResponse {
  results: Vec<PoolAssignItem>,
}

#[derive(Debug, Serialize, ToSchema)]
struct RotateProfileProxyResponse {
  /// The proxy the profile now points at.
  proxy_id: String,
  proxy_settings: ProxySettings,
}

/// REST request for the LLM completion endpoint.
#[derive(Debug, Deserialize, ToSchema)]
struct ApiLlmCompletionRequest {
  /// Saved key id from the key vault. When omitted, the first saved key is used.
  key_id: Option<String>,
  /// Provider override. When omitted, the key's own provider is used.
  provider: Option<String>,
  /// Model override. When omitted, the key's configured model is used.
  model: Option<String>,
  /// Chat history (roles: system/user/assistant).
  messages: Vec<crate::llm::ChatMessage>,
  /// Optional function-calling tools.
  #[serde(default)]
  tools: Option<Vec<crate::llm::ToolSpec>>,
  /// Retries on transient failures (429/5xx/transport). Defaults to 3.
  #[serde(default)]
  max_retries: Option<u32>,
}

impl From<ApiLlmCompletionRequest> for crate::llm_completion::LlmCompletionRequest {
  fn from(request: ApiLlmCompletionRequest) -> Self {
    Self {
      key_id: request.key_id,
      provider: request.provider,
      model: request.model,
      messages: request.messages,
      tools: request.tools,
      max_retries: request.max_retries,
    }
  }
}

/// REST response for the LLM completion endpoint.
#[derive(Debug, Serialize, ToSchema)]
struct ApiLlmCompletionResponse {
  reply: String,
  usage: Option<crate::llm::ChatUsage>,
  /// Provider actually used (after overrides).
  provider: String,
  /// Model actually used (after overrides).
  model: String,
}

impl From<crate::llm_completion::LlmCompletionResult> for ApiLlmCompletionResponse {
  fn from(result: crate::llm_completion::LlmCompletionResult) -> Self {
    Self {
      reply: result.reply,
      usage: result.usage,
      provider: result.provider,
      model: result.model,
    }
  }
}

#[derive(Debug, Deserialize, ToSchema)]
struct BatchCreateProfilesRequest {
  /// Explicit profile names. When non-empty, `name_prefix`/`count` are ignored.
  #[serde(default)]
  names: Vec<String>,
  /// Generate `"{prefix} {i}"` names for i in 1..=count. Used when `names` is empty.
  name_prefix: Option<String>,
  /// Number of profiles to create when using `name_prefix`.
  count: Option<u32>,
  /// Browser engine. Must be `"chromium"`.
  browser: String,
  /// Omit (or pass `"latest"`) to use the newest already-downloaded version.
  #[serde(default)]
  version: Option<String>,
  /// Defaults to "stable".
  release_type: Option<String>,
  proxy_id: Option<String>,
  vpn_id: Option<String>,
  launch_hook: Option<String>,
  /// Shared browser config for every profile in the batch. Omit to have one
  /// fingerprint generated once and shared; provide a `fingerprint` field to
  /// pin one for the whole batch.
  #[schema(value_type = Option<Object>)]
  chromium_config: Option<serde_json::Value>,
  group_id: Option<String>,
  #[serde(default)]
  ephemeral: bool,
  dns_blocklist: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
struct BatchCreateProfileResult {
  name: String,
  /// Whether this profile was created successfully.
  ok: bool,
  /// The created profile, when successful.
  #[schema(value_type = Option<ApiProfile>)]
  profile: Option<ApiProfile>,
  /// Failure reason when not created.
  error: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
struct BatchCreateProfilesResponse {
  results: Vec<BatchCreateProfileResult>,
  /// Number of requested profiles (not successes).
  total: usize,
}

#[derive(Debug, Serialize, ToSchema)]
struct DetectedProfilesResponse {
  profiles: Vec<crate::profile_importer::DetectedProfile>,
  total: usize,
}

#[derive(Debug, Deserialize)]
struct DetectImportQuery {
  /// Optional folder to scan instead of the default browser locations.
  folder: Option<String>,
}

#[derive(Debug, Deserialize, ToSchema)]
struct ImportProfilesRequest {
  /// Profiles to import. Each item is isolated — one failure doesn't stop the rest.
  items: Vec<crate::profile_importer::ImportProfileItem>,
  /// Optional group to assign every imported profile to.
  group_id: Option<String>,
  /// How to handle an already-taken profile name: "skip" or "rename"
  /// (auto-suffix). Defaults to "rename".
  duplicate_strategy: Option<crate::profile_importer::DuplicateStrategy>,
  /// Browser fingerprint/config applied to every imported profile. Omit to
  /// have fresh fingerprints generated automatically.
  #[schema(value_type = Option<Object>)]
  chromium_config: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize, ToSchema)]
struct ImportProxiesRequest {
  /// "txt" — one proxy per line (`host:port`, `host:port:user:pass`, or URL
  /// forms like `http://user:pass@host:port`). "json" — a Duckling proxy export.
  format: String,
  /// Raw proxy list / export content.
  content: String,
  /// Name prefix for txt imports; proxies are named "{prefix} Proxy {n}".
  name_prefix: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
struct ImportProxiesResponse {
  imported_count: usize,
  skipped_count: usize,
  errors: Vec<String>,
  proxies: Vec<ApiProxyResponse>,
}

#[derive(OpenApi)]
#[openapi(
  paths(
    get_profiles,
    get_profile,
    create_profile,
    update_profile,
    delete_profile,
    run_profile,
    open_url_in_profile,
    kill_profile,
    batch_run_profiles,
    batch_stop_profiles,
    batch_create_profiles_api,
    create_proxy_pool_api,
    list_proxy_pools_api,
    update_proxy_pool_api,
    delete_proxy_pool_api,
    assign_profiles_to_pool_api,
    rotate_profile_proxy_api,
    llm_completion_api,
    system_status_api,
    detect_import_profiles,
    import_profiles_api,
    import_profile_cookies,
    get_groups,
    get_group,
    create_group,
    update_group,
    delete_group,
    get_tags,
    get_proxies,
    get_proxy,
    create_proxy,
    import_proxies_api,
    update_proxy,
    delete_proxy,
    get_vpns,
    get_vpn,
    export_vpn,
    import_vpn,
    create_vpn,
    update_vpn,
    delete_vpn,
    get_extensions,
    get_extension_groups,
    delete_extension_api,
    delete_extension_group_api,
    download_browser_api,
    get_browser_versions,
    check_browser_downloaded,
  ),
  components(schemas(
    ApiProfile,
    ApiProfilesResponse,
    ApiProfileResponse,
    CreateProfileRequest,
    UpdateProfileRequest,
    ApiGroupResponse,
    CreateGroupRequest,
    UpdateGroupRequest,
    ApiProxyResponse,
    CreateProxyRequest,
    UpdateProxyRequest,
    ApiVpnResponse,
    ApiVpnExportResponse,
    ImportVpnRequest,
    CreateVpnRequest,
    UpdateVpnRequest,
    DownloadBrowserRequest,
    DownloadBrowserResponse,
    RunProfileResponse,
    RunProfileRequest,
    BatchRunRequest,
    BatchRunResult,
    BatchRunResponse,
    BatchStopRequest,
    BatchStopResult,
    BatchStopResponse,
    BatchCreateProfilesRequest,
    BatchCreateProfileResult,
    BatchCreateProfilesResponse,
    CreateProxyPoolRequest,
    ApiProxyPoolResponse,
    UpdateProxyPoolRequest,
    AssignProfilesToPoolRequest,
    PoolAssignItem,
    AssignProfilesToPoolResponse,
    RotateProfileProxyResponse,
    ApiLlmCompletionRequest,
    ApiLlmCompletionResponse,
    crate::launch_scheduler::LaunchSchedulerMetrics,
    OpenUrlRequest,
    ImportCookiesRequest,
    ImportCookiesResponse,
    ProxySettings,
    DetectedProfilesResponse,
    ImportProfilesRequest,
    ImportProxiesRequest,
    ImportProxiesResponse,
    crate::profile_importer::DetectedProfile,
    crate::profile_importer::ImportProfileItem,
    crate::profile_importer::DuplicateStrategy,
    crate::profile_importer::ProfileImportItemResult,
    crate::profile_importer::ProfileImportBatchResult,
  )),
  tags(
    (name = "profiles", description = "Profile management endpoints"),
    (name = "groups", description = "Group management endpoints"),
    (name = "tags", description = "Tag management endpoints"),
    (name = "proxies", description = "Proxy management endpoints"),
    (name = "proxy-pools", description = "Proxy pool management endpoints"),
    (name = "llm", description = "LLM completion endpoints (retry + per-provider concurrency)"),
    (name = "vpns", description = "VPN management endpoints"),
    (name = "extensions", description = "Extension management endpoints"),
    (name = "browsers", description = "Browser management endpoints"),
    (name = "cookies", description = "Cookie management endpoints"),
    (name = "system", description = "System and scheduler endpoints"),
  ),
  modifiers(&SecurityAddon),
)]
struct ApiDoc;

struct SecurityAddon;

impl utoipa::Modify for SecurityAddon {
  fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
    if let Some(components) = openapi.components.as_mut() {
      components.add_security_scheme(
        "bearer_auth",
        utoipa::openapi::security::SecurityScheme::Http(
          utoipa::openapi::security::HttpBuilder::new()
            .scheme(utoipa::openapi::security::HttpAuthScheme::Bearer)
            .bearer_format("JWT")
            .build(),
        ),
      );
    }
  }
}

pub struct ApiServer {
  port: Option<u16>,
  shutdown_tx: Option<mpsc::Sender<()>>,
  task_handle: Option<tokio::task::JoinHandle<()>>,
}

impl ApiServer {
  fn new() -> Self {
    Self {
      port: None,
      shutdown_tx: None,
      task_handle: None,
    }
  }

  fn get_port(&self) -> Option<u16> {
    self.port
  }

  async fn start(
    &mut self,
    app_handle: tauri::AppHandle,
    preferred_port: u16,
  ) -> Result<u16, String> {
    // Stop existing server if running
    self.stop().await.ok();

    let (shutdown_tx, mut shutdown_rx) = mpsc::channel(1);
    let state = ApiServerState {
      app_handle: app_handle.clone(),
    };

    // Try preferred port first, then random port
    let listener = match TcpListener::bind(format!("127.0.0.1:{preferred_port}")).await {
      Ok(listener) => listener,
      Err(_) => {
        // Port conflict, try random port
        let random_port = rand::random::<u16>().saturating_add(10000);
        match TcpListener::bind(format!("127.0.0.1:{random_port}")).await {
          Ok(listener) => {
            let _ = events::emit(
              "api-port-conflict",
              format!("API server using fallback port {random_port}"),
            );
            listener
          }
          Err(e) => {
            return Err(crate::backend_error_with_detail("API_PORT_UNAVAILABLE", e));
          }
        }
      }
    };

    let actual_port = listener
      .local_addr()
      .map_err(|e| crate::backend_error_with_detail("INTERNAL_ERROR", e))?
      .port();

    // Create router with OpenAPI documentation
    let (v1_routes, _) = OpenApiRouter::new()
      .routes(routes!(get_profiles, create_profile))
      .routes(routes!(get_profile, update_profile, delete_profile))
      .routes(routes!(run_profile))
      .routes(routes!(open_url_in_profile))
      .routes(routes!(kill_profile))
      .routes(routes!(batch_run_profiles))
      .routes(routes!(batch_stop_profiles))
      .routes(routes!(batch_create_profiles_api))
      .routes(routes!(create_proxy_pool_api, list_proxy_pools_api))
      .routes(routes!(update_proxy_pool_api, delete_proxy_pool_api))
      .routes(routes!(
        assign_profiles_to_pool_api,
        rotate_profile_proxy_api
      ))
      .routes(routes!(llm_completion_api))
      .routes(routes!(system_status_api))
      .routes(routes!(detect_import_profiles))
      .routes(routes!(import_profiles_api))
      .routes(routes!(import_profile_cookies))
      .routes(routes!(get_groups, create_group))
      .routes(routes!(get_group, update_group, delete_group))
      .routes(routes!(get_tags))
      .routes(routes!(get_proxies, create_proxy))
      .routes(routes!(import_proxies_api))
      .routes(routes!(get_proxy, update_proxy, delete_proxy))
      .routes(routes!(get_vpns, create_vpn))
      .routes(routes!(import_vpn))
      .routes(routes!(export_vpn))
      .routes(routes!(get_vpn, update_vpn, delete_vpn))
      .routes(routes!(get_extensions))
      .routes(routes!(delete_extension_api))
      .routes(routes!(get_extension_groups))
      .routes(routes!(delete_extension_group_api))
      .routes(routes!(download_browser_api))
      .routes(routes!(get_browser_versions))
      .routes(routes!(check_browser_downloaded))
      .split_for_parts();

    let api = ApiDoc::openapi();

    let v1_routes = v1_routes
      // Innermost so only authenticated automation requests consume quota.
      .layer(middleware::from_fn(rate_limit_middleware))
      .layer(middleware::from_fn_with_state(
        state.clone(),
        auth_middleware,
      ))
      .layer(middleware::from_fn(terms_check_middleware));

    let api_for_v1 = api.clone();
    let app = Router::new()
      .merge(v1_routes)
      .route("/openapi.json", get(move || async move { Json(api) }))
      .route(
        "/v1/openapi.json",
        get(move || async move { Json(api_for_v1) }),
      )
      // Outermost layer: logs every request so customer reports show what
      // their automation is actually calling, what the response status was,
      // and how long it took. Never logs request bodies or auth headers.
      .layer(middleware::from_fn(request_logging_middleware))
      .layer(CorsLayer::permissive())
      .with_state(state);

    // Start server task
    let task_handle = tokio::spawn(async move {
      let server = axum::serve(listener, app);
      tokio::select! {
        _ = server => {},
        _ = shutdown_rx.recv() => {},
      }
    });

    self.port = Some(actual_port);
    self.shutdown_tx = Some(shutdown_tx);
    self.task_handle = Some(task_handle);

    Ok(actual_port)
  }

  async fn stop(&mut self) -> Result<(), String> {
    if let Some(shutdown_tx) = self.shutdown_tx.take() {
      let _ = shutdown_tx.send(()).await;
    }

    if let Some(handle) = self.task_handle.take() {
      handle.abort();
    }

    self.port = None;
    Ok(())
  }
}

// Terms and Conditions check middleware
async fn terms_check_middleware(
  request: axum::extract::Request,
  next: Next,
) -> Result<Response, StatusCode> {
  // Check if browser terms have been accepted
  if !crate::chromium_terms::ChromiumTermsManager::instance().is_terms_accepted() {
    return Err(StatusCode::FORBIDDEN);
  }

  Ok(next.run(request).await)
}

// Authentication middleware
async fn auth_middleware(
  State(state): State<ApiServerState>,
  headers: HeaderMap,
  request: axum::extract::Request,
  next: Next,
) -> Result<Response, StatusCode> {
  let path = request.uri().path().to_string();

  // Get the Authorization header
  let auth_header = headers
    .get("Authorization")
    .and_then(|h| h.to_str().ok())
    .and_then(|h| h.strip_prefix("Bearer "));

  let token = match auth_header {
    Some(token) => token,
    None => {
      log::warn!("[api] Rejected {path}: missing Authorization header");
      return Err(StatusCode::UNAUTHORIZED);
    }
  };

  // Get the stored token
  let settings_manager = crate::settings_manager::SettingsManager::instance();
  let stored_token = match settings_manager.get_api_token(&state.app_handle).await {
    Ok(Some(stored_token)) => stored_token,
    Ok(None) => {
      log::warn!(
        "[api] Rejected {path}: API server has no stored token (was the API toggled off?)"
      );
      return Err(StatusCode::UNAUTHORIZED);
    }
    Err(e) => {
      log::error!("[api] Failed to read stored API token: {e}");
      return Err(StatusCode::INTERNAL_SERVER_ERROR);
    }
  };

  // Constant-time comparison so the auth check doesn't leak the shared-prefix
  // length via timing. `ConstantTimeEq` on equal-length byte slices; differing
  // lengths simply compare unequal.
  use subtle::ConstantTimeEq;
  let token_bytes = token.as_bytes();
  let stored_bytes = stored_token.as_bytes();
  let matches = token_bytes.len() == stored_bytes.len() && token_bytes.ct_eq(stored_bytes).into();
  if !matches {
    log::warn!("[api] Rejected {path}: token mismatch");
    return Err(StatusCode::UNAUTHORIZED);
  }

  // Token is valid, continue with the request
  Ok(next.run(request).await)
}

/// Logs every request: method, path, query, response status, duration.
/// Skips Authorization header and request bodies entirely.
async fn request_logging_middleware(request: axum::extract::Request, next: Next) -> Response {
  let method = request.method().clone();
  let path = request.uri().path().to_string();
  let query = request.uri().query().map(|q| q.to_string());
  let started = std::time::Instant::now();

  let response = next.run(request).await;

  let status = response.status();
  let elapsed_ms = started.elapsed().as_millis();

  let level = if status.is_server_error() {
    log::Level::Error
  } else if status.is_client_error() {
    log::Level::Warn
  } else {
    log::Level::Info
  };

  match query {
    Some(q) => log::log!(
      level,
      "[api] {method} {path}?{q} -> {status} ({elapsed_ms} ms)"
    ),
    None => log::log!(level, "[api] {method} {path} -> {status} ({elapsed_ms} ms)"),
  }

  response
}

fn is_automation_request(method: &Method, path: &str) -> bool {
  if method != Method::POST {
    return false;
  }

  if matches!(path, "/v1/profiles/batch/run" | "/v1/profiles/batch/stop") {
    return true;
  }

  let Some(profile_action) = path.strip_prefix("/v1/profiles/") else {
    return false;
  };
  let mut segments = profile_action.split('/');
  matches!(
    (segments.next(), segments.next(), segments.next()),
    (Some(_), Some("run" | "open-url" | "kill"), None)
  )
}

async fn rate_limit_middleware(request: axum::extract::Request, next: Next) -> Response {
  if !is_automation_request(request.method(), request.uri().path()) {
    return next.run(request).await;
  }

  match crate::automation_rate_limiter::check_automation_rate_limit().await {
    crate::automation_rate_limiter::RateLimitOutcome::Limited { retry_after_secs } => {
      log::warn!(
        "[api] Rejected {}: automation rate limit exceeded; retry in {}s",
        request.uri().path(),
        retry_after_secs
      );
      (
        StatusCode::TOO_MANY_REQUESTS,
        [(header::RETRY_AFTER, retry_after_secs.to_string())],
        "automation request rate limit exceeded",
      )
        .into_response()
    }
    crate::automation_rate_limiter::RateLimitOutcome::Unlimited
    | crate::automation_rate_limiter::RateLimitOutcome::Allowed { .. } => next.run(request).await,
  }
}

// Global API server instance
lazy_static! {
  pub static ref API_SERVER: Arc<Mutex<ApiServer>> = Arc::new(Mutex::new(ApiServer::new()));
}

// Tauri commands
#[tauri::command]
pub async fn start_api_server_internal(
  port: u16,
  app_handle: &tauri::AppHandle,
) -> Result<u16, String> {
  let mut server_guard = API_SERVER.lock().await;
  server_guard.start(app_handle.clone(), port).await
}

#[tauri::command]
pub async fn stop_api_server() -> Result<(), String> {
  let mut server_guard = API_SERVER.lock().await;
  server_guard.stop().await
}

#[tauri::command]
pub async fn start_api_server(
  port: Option<u16>,
  app_handle: tauri::AppHandle,
) -> Result<u16, String> {
  let actual_port = port.unwrap_or(10108);
  start_api_server_internal(actual_port, &app_handle).await
}

#[tauri::command]
pub async fn get_api_server_status() -> Result<Option<u16>, String> {
  let server_guard = API_SERVER.lock().await;
  Ok(server_guard.get_port())
}

// API Handlers - Profiles
/// Maps a manager-layer error onto a consistent HTTP status: 404 for missing
/// entities, 400 for validation/duplicate/client-input errors, 500 for
/// everything else (IO and other internal failures). The error text passes
/// through as the response body so API clients get a diagnostic instead of a
/// bare status code. Matching is on message content because the managers
/// return plain strings (some are the JSON `{"code": ...}` strings shared
/// with the Tauri commands).
fn manager_error_response(err: impl std::fmt::Display) -> (StatusCode, String) {
  let msg = err.to_string();

  // Structured {"code": ...} errors from the shared managers classify exactly.
  if let Ok(value) = serde_json::from_str::<serde_json::Value>(&msg) {
    if let Some(code) = value.get("code").and_then(|c| c.as_str()) {
      let status = if code.ends_with("_NOT_FOUND") {
        StatusCode::NOT_FOUND
      } else if code == "INTERNAL_ERROR" {
        StatusCode::INTERNAL_SERVER_ERROR
      } else if code.ends_with("_REQUIRES_PRO") || code.ends_with("_PAYMENT_REQUIRED") {
        // Paid-feature gates (FINGERPRINT_REQUIRES_PRO, PROXY_PAYMENT_REQUIRED).
        // Mapping them here lets the gate live in the shared manager instead of
        // being re-implemented in each handler to get the status right.
        StatusCode::PAYMENT_REQUIRED
      } else {
        // Validation-style codes (NAME_CANNOT_BE_EMPTY, GROUP_ALREADY_EXISTS,
        // CHROMIUM_VERSION_NOT_AVAILABLE, ...).
        StatusCode::BAD_REQUEST
      };
      return (status, msg);
    }
  }

  // Plain-text manager messages: match the known phrases narrowly so raw
  // OS/serde/network error text (e.g. "invalid type: ..." from a corrupt
  // store) falls through to 500 instead of masquerading as a client error.
  let lower = msg.to_lowercase();
  let status = if lower.contains("not found") {
    StatusCode::NOT_FOUND
  } else if lower.contains("already exists")
    || lower.contains("cannot set both")
    || lower.contains("cannot edit")
    || lower.contains("cannot delete")
    || lower.contains("cannot open url")
    || lower.contains("invalid browser")
    || lower.contains("invalid profile id")
    || lower.contains("unsupported browser")
    || lower.contains("not supported on your platform")
    || lower.contains("is not downloaded")
    || lower.contains("terms and conditions")
  {
    StatusCode::BAD_REQUEST
  } else {
    StatusCode::INTERNAL_SERVER_ERROR
  };
  (status, msg)
}

/// Real per-group profile counts, computed from the profile list (the same
/// source of truth the GUI uses).
fn group_profile_counts() -> std::collections::HashMap<String, usize> {
  let mut counts = std::collections::HashMap::new();
  if let Ok(profiles) = ProfileManager::instance().list_profiles() {
    for profile in profiles {
      if let Some(group_id) = profile.group_id {
        *counts.entry(group_id).or_insert(0) += 1;
      }
    }
  }
  counts
}

#[utoipa::path(
  get,
  path = "/v1/profiles",
  responses(
    (status = 200, description = "List of all profiles", body = ApiProfilesResponse),
    (status = 401, description = "Unauthorized"),
    (status = 500, description = "Internal server error")
  ),
  security(
    ("bearer_auth" = [])
  ),
  tag = "profiles"
)]
async fn get_profiles() -> Result<Json<ApiProfilesResponse>, StatusCode> {
  let profile_manager = ProfileManager::instance();
  match profile_manager.list_profiles() {
    Ok(profiles) => {
      let api_profiles: Vec<ApiProfile> = profiles
        .iter()
        .map(|profile| ApiProfile {
          id: profile.id.to_string(),
          name: profile.name.clone(),
          browser: profile.browser.clone(),
          version: profile.version.clone(),
          proxy_id: profile.proxy_id.clone(),
          launch_hook: profile.launch_hook.clone(),
          process_id: profile.process_id,
          last_launch: profile.last_launch,
          release_type: profile.release_type.clone(),
          group_id: profile.group_id.clone(),
          tags: profile.tags.clone(),
          is_running: profile.process_id.is_some(), // Simple check based on process_id
          proxy_bypass_rules: profile.proxy_bypass_rules.clone(),
          vpn_id: profile.vpn_id.clone(),
          clear_on_close: profile.clear_on_close,
        })
        .collect();

      Ok(Json(ApiProfilesResponse {
        profiles: api_profiles,
        total: profiles.len(),
      }))
    }
    Err(_) => Err(StatusCode::INTERNAL_SERVER_ERROR),
  }
}

#[utoipa::path(
  get,
  path = "/v1/profiles/{id}",
  params(
    ("id" = String, Path, description = "Profile ID")
  ),
  responses(
    (status = 200, description = "Profile details", body = ApiProfileResponse),
    (status = 401, description = "Unauthorized"),
    (status = 404, description = "Profile not found"),
    (status = 500, description = "Internal server error")
  ),
  security(
    ("bearer_auth" = [])
  ),
  tag = "profiles"
)]
async fn get_profile(
  Path(id): Path<String>,
  State(_state): State<ApiServerState>,
) -> Result<Json<ApiProfileResponse>, StatusCode> {
  let profile_manager = ProfileManager::instance();
  let Ok(profile_id) = id.parse::<uuid::Uuid>() else {
    return Err(StatusCode::NOT_FOUND);
  };
  match profile_manager.get_profile_by_id(&profile_id) {
    Ok(Some(profile)) => {
      Ok(Json(ApiProfileResponse {
        profile: ApiProfile {
          id: profile.id.to_string(),
          name: profile.name.clone(),
          browser: profile.browser.clone(),
          version: profile.version.clone(),
          proxy_id: profile.proxy_id.clone(),
          launch_hook: profile.launch_hook.clone(),
          process_id: profile.process_id,
          last_launch: profile.last_launch,
          release_type: profile.release_type.clone(),
          group_id: profile.group_id.clone(),
          tags: profile.tags.clone(),
          is_running: profile.process_id.is_some(), // Simple check based on process_id
          proxy_bypass_rules: profile.proxy_bypass_rules.clone(),
          vpn_id: profile.vpn_id.clone(),
          clear_on_close: profile.clear_on_close,
        },
      }))
    }
    Ok(None) => Err(StatusCode::NOT_FOUND),
    Err(_) => Err(StatusCode::INTERNAL_SERVER_ERROR),
  }
}

/// Create a profile.
///
/// - `browser` must be `"chromium"`; any other value is rejected
///   with 400.
/// - `version` is optional: omit it or pass `"latest"` to use the newest
///   already-downloaded version of that browser. The version must be present
///   locally (this endpoint does not download new versions); 400 if none is.
/// - Omitting the matching `chromium_config`, or passing an
///   empty object `{}`, generates a fresh fingerprint automatically.
#[utoipa::path(
  post,
  path = "/v1/profiles",
  request_body = CreateProfileRequest,
  responses(
    (status = 200, description = "Profile created successfully", body = ApiProfileResponse),
    (status = 400, description = "Invalid browser, or no downloaded version available"),
    (status = 401, description = "Unauthorized"),
    (status = 402, description = "Selected proxy requires payment"),
    (status = 500, description = "Internal server error")
  ),
  security(
    ("bearer_auth" = [])
  ),
  tag = "profiles"
)]
async fn create_profile(
  State(state): State<ApiServerState>,
  Json(request): Json<CreateProfileRequest>,
) -> Result<Json<ApiProfileResponse>, (StatusCode, String)> {
  let profile_manager = ProfileManager::instance();

  // Only Chromium profiles are launchable; the rest of the system
  // (fingerprint generation, launch, run) supports nothing else. Reject anything
  // else up front — otherwise the profile is created with no fingerprint and an
  // unrecognized browser, then crashes with a 500 on /run. Mirrors the MCP
  // create_profile validation.
  if request.browser != "chromium" {
    return Err((
      StatusCode::BAD_REQUEST,
      format!(
        "Invalid browser \"{}\". Must be \"Chromium\" (Chromium-based).",
        request.browser
      ),
    ));
  }

  // Resolve the version. Omitted, empty, or "latest" means "newest version
  // already downloaded for this browser". The create path generates the
  // fingerprint by launching that binary, so the version must be present
  // locally — we don't fetch new versions here. 400 if none is downloaded.
  let version = match request.version.as_deref() {
    Some(v) if !v.is_empty() && v != "latest" => v.to_string(),
    _ => {
      let registry = crate::downloaded_browsers_registry::DownloadedBrowsersRegistry::instance();
      let mut versions = registry.get_downloaded_versions(&request.browser);
      // browsers is a HashMap, so keys are unordered — sort newest-first by
      // semver before taking the latest.
      versions.sort_by(|a, b| crate::api_client::compare_versions(b, a));
      match versions.into_iter().next() {
        Some(v) => v,
        None => {
          return Err((
            StatusCode::BAD_REQUEST,
            format!(
              "No downloaded version of \"{}\" is available. Download the browser in Duckling Browser first — this endpoint does not download browsers.",
              request.browser
            ),
          ));
        }
      }
    }
  };

  // Parse Chromium config if provided
  let chromium_config = if let Some(config) = &request.chromium_config {
    serde_json::from_value(config.clone()).ok()
  } else {
    None
  };

  // Reject a dead/unreachable proxy or VPN before creating the profile. A 402
  // (expired proxy subscription) maps to 402; anything else is a 400.
  if let Err(err) =
    crate::validate_profile_network(request.proxy_id.as_deref(), request.vpn_id.as_deref()).await
  {
    return Err(if err.contains("PROXY_PAYMENT_REQUIRED") {
      (
        StatusCode::PAYMENT_REQUIRED,
        "The selected proxy requires an active subscription.".to_string(),
      )
    } else {
      (
        StatusCode::BAD_REQUEST,
        format!("Profile network validation failed: {err}"),
      )
    });
  }

  // Create profile using the async create_profile_with_group method
  match profile_manager
    .create_profile_with_group(
      &state.app_handle,
      &request.name,
      &request.browser,
      &version,
      request.release_type.as_deref().unwrap_or("stable"),
      request.proxy_id.clone(),
      request.vpn_id.clone(),
      chromium_config,
      request.group_id.clone(),
      false,
      None,
      request.launch_hook.clone(),
    )
    .await
  {
    Ok(mut profile) => {
      // Apply tags if provided
      if let Some(tags) = &request.tags {
        if profile_manager
          .update_profile_tags(&state.app_handle, &profile.name, tags.clone())
          .is_err()
        {
          return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            "Profile created but failed to apply tags.".to_string(),
          ));
        }
        profile.tags = tags.clone();
      }

      // Update tag manager with new tags
      if let Ok(profiles) = profile_manager.list_profiles() {
        let _ = crate::tag_manager::TAG_MANAGER
          .lock()
          .map(|manager| manager.rebuild_from_profiles(&profiles));
      }

      Ok(Json(ApiProfileResponse {
        profile: ApiProfile {
          id: profile.id.to_string(),
          name: profile.name,
          browser: profile.browser,
          version: profile.version,
          proxy_id: profile.proxy_id,
          launch_hook: profile.launch_hook,
          process_id: profile.process_id,
          last_launch: profile.last_launch,
          release_type: profile.release_type,
          group_id: profile.group_id,
          tags: profile.tags,
          is_running: false,
          proxy_bypass_rules: profile.proxy_bypass_rules,
          vpn_id: profile.vpn_id,
          clear_on_close: profile.clear_on_close,
        },
      }))
    }
    Err(e) => Err((
      StatusCode::BAD_REQUEST,
      format!("Failed to create profile: {e}"),
    )),
  }
}

/// Resolve the version for batch creation: concrete value, or the newest
/// already-downloaded version when omitted/empty/"latest". Mirrors
/// `create_profile`'s resolution (the create path generates the fingerprint
/// from a locally available binary, so nothing is downloaded here).
fn resolve_batch_version(
  request: &BatchCreateProfilesRequest,
) -> Result<String, (StatusCode, String)> {
  match request.version.as_deref() {
    Some(v) if !v.is_empty() && v != "latest" => Ok(v.to_string()),
    _ => {
      let registry = crate::downloaded_browsers_registry::DownloadedBrowsersRegistry::instance();
      let mut versions = registry.get_downloaded_versions(&request.browser);
      versions.sort_by(|a, b| crate::api_client::compare_versions(b, a));
      match versions.into_iter().next() {
        Some(v) => Ok(v),
        None => Err((
          StatusCode::BAD_REQUEST,
          format!(
            "No downloaded version of \"{}\" is available. Download the browser in Duckling Browser first — this endpoint does not download browsers.",
            request.browser
          ),
        )),
      }
    }
  }
}

// API Handler - Create many profiles from one shared template in a single
// call. Creation is not browser automation, so this route is NOT in the
// automation rate-limit bucket (mirrors POST /v1/profiles).
#[utoipa::path(
  post,
  path = "/v1/profiles/batch/create",
  request_body = BatchCreateProfilesRequest,
  responses(
    (status = 200, description = "Batch creation completed; inspect per-profile results", body = BatchCreateProfilesResponse),
    (status = 400, description = "Invalid browser, no downloaded version available, or no names"),
    (status = 401, description = "Unauthorized"),
    (status = 500, description = "Internal server error")
  ),
  security(
    ("bearer_auth" = [])
  ),
  tag = "profiles"
)]
async fn batch_create_profiles_api(
  State(state): State<ApiServerState>,
  Json(request): Json<BatchCreateProfilesRequest>,
) -> Result<Json<BatchCreateProfilesResponse>, (StatusCode, String)> {
  let profile_manager = ProfileManager::instance();

  if request.browser != "chromium" {
    return Err((
      StatusCode::BAD_REQUEST,
      format!(
        "Invalid browser \"{}\". Must be \"Chromium\" (Chromium-based).",
        request.browser
      ),
    ));
  }

  let version = resolve_batch_version(&request)?;

  let chromium_config = if let Some(config) = &request.chromium_config {
    serde_json::from_value(config.clone()).ok()
  } else {
    None
  };

  // A dead/unreachable proxy or VPN cancels the whole batch before any
  // profile is created (validated once, not per name).
  if let Err(err) =
    crate::validate_profile_network(request.proxy_id.as_deref(), request.vpn_id.as_deref()).await
  {
    return Err(if err.contains("PROXY_PAYMENT_REQUIRED") {
      (
        StatusCode::PAYMENT_REQUIRED,
        "The selected proxy requires an active subscription.".to_string(),
      )
    } else {
      (
        StatusCode::BAD_REQUEST,
        format!("Profile network validation failed: {err}"),
      )
    });
  }

  let manager_request = crate::profile::manager::BatchCreateProfilesRequest {
    names: request.names,
    name_prefix: request.name_prefix,
    count: request.count,
    browser: request.browser,
    version,
    release_type: request.release_type.unwrap_or_else(|| "stable".to_string()),
    proxy_id: request.proxy_id,
    vpn_id: request.vpn_id,
    chromium_config,
    group_id: request.group_id,
    ephemeral: request.ephemeral,
    dns_blocklist: request.dns_blocklist,
    launch_hook: request.launch_hook,
  };

  let results = match profile_manager
    .batch_create_profiles(&state.app_handle, &manager_request)
    .await
  {
    Ok(results) => results,
    Err(e) => {
      return Err((
        StatusCode::BAD_REQUEST,
        format!("Failed to batch create profiles: {e}"),
      ));
    }
  };

  let total = results.len();
  let results: Vec<BatchCreateProfileResult> = results
    .into_iter()
    .map(|result| BatchCreateProfileResult {
      name: result.name,
      ok: result.ok,
      profile: result.profile.map(|p| ApiProfile {
        id: p.id.to_string(),
        name: p.name,
        browser: p.browser,
        version: p.version,
        proxy_id: p.proxy_id,
        launch_hook: p.launch_hook,
        process_id: p.process_id,
        last_launch: p.last_launch,
        release_type: p.release_type,
        group_id: p.group_id,
        tags: p.tags,
        is_running: false,
        proxy_bypass_rules: p.proxy_bypass_rules,
        vpn_id: p.vpn_id,
        clear_on_close: p.clear_on_close,
      }),
      error: result.error,
    })
    .collect();

  Ok(Json(BatchCreateProfilesResponse { results, total }))
}

#[utoipa::path(
  put,
  path = "/v1/profiles/{id}",
  params(
    ("id" = String, Path, description = "Profile ID")
  ),
  request_body = UpdateProfileRequest,
  responses(
    (status = 200, description = "Profile updated successfully", body = ApiProfileResponse),
    (status = 400, description = "Bad request"),
    (status = 401, description = "Unauthorized"),
    (status = 404, description = "Profile not found"),
    (status = 500, description = "Internal server error")
  ),
  security(
    ("bearer_auth" = [])
  ),
  tag = "profiles"
)]
async fn update_profile(
  Path(id): Path<String>,
  State(state): State<ApiServerState>,
  Json(request): Json<UpdateProfileRequest>,
) -> Result<Json<ApiProfileResponse>, (StatusCode, String)> {
  let profile_manager = ProfileManager::instance();

  if request.proxy_id.as_deref().is_some_and(|s| !s.is_empty())
    && request.vpn_id.as_deref().is_some_and(|s| !s.is_empty())
  {
    return Err((
      StatusCode::BAD_REQUEST,
      "Cannot set both proxy_id and vpn_id".to_string(),
    ));
  }

  // Update profile fields
  if let Some(new_name) = request.name {
    if let Err(e) = profile_manager.rename_profile(&state.app_handle, &id, &new_name) {
      return Err(manager_error_response(e));
    }
  }

  if let Some(version) = request.version {
    if let Err(e) = profile_manager.update_profile_version(&state.app_handle, &id, &version) {
      return Err(manager_error_response(e));
    }
  }

  if let Some(proxy_id) = request.proxy_id {
    if let Err(e) = profile_manager
      .update_profile_proxy(state.app_handle.clone(), &id, Some(proxy_id))
      .await
    {
      return Err(manager_error_response(e));
    }
  }

  if let Some(vpn_id) = request.vpn_id {
    let normalized = if vpn_id.is_empty() {
      None
    } else {
      Some(vpn_id)
    };
    if let Err(e) = profile_manager
      .update_profile_vpn(state.app_handle.clone(), &id, normalized)
      .await
    {
      return Err(manager_error_response(e));
    }
  }

  if let Some(launch_hook) = request.launch_hook {
    let normalized = if launch_hook.trim().is_empty() {
      None
    } else {
      Some(launch_hook)
    };

    if let Err(e) = profile_manager.update_profile_launch_hook(&state.app_handle, &id, normalized) {
      return Err(manager_error_response(e));
    }
  }

  if let Some(group_id) = request.group_id {
    if let Err(e) =
      profile_manager.assign_profiles_to_group(&state.app_handle, vec![id.clone()], Some(group_id))
    {
      return Err(manager_error_response(e));
    }
  }

  if let Some(tags) = request.tags {
    if let Err(e) = profile_manager.update_profile_tags(&state.app_handle, &id, tags) {
      return Err(manager_error_response(e));
    }

    // Update tag manager with new tags from all profiles
    if let Ok(profiles) = profile_manager.list_profiles() {
      let _ = crate::tag_manager::TAG_MANAGER
        .lock()
        .map(|manager| manager.rebuild_from_profiles(&profiles));
    }
  }

  if let Some(extension_group_id) = request.extension_group_id {
    let ext_group = if extension_group_id.is_empty() {
      None
    } else {
      Some(extension_group_id)
    };
    if let Err(e) = profile_manager.update_profile_extension_group(&id, ext_group) {
      return Err(manager_error_response(e));
    }
  }

  if let Some(proxy_bypass_rules) = request.proxy_bypass_rules {
    if let Err(e) =
      profile_manager.update_profile_proxy_bypass_rules(&state.app_handle, &id, proxy_bypass_rules)
    {
      return Err(manager_error_response(e));
    }
  }

  if let Some(sync_mode) = request.sync_mode {
    if let Err(e) =
      crate::sync::set_profile_sync_mode(state.app_handle.clone(), id.clone(), sync_mode).await
    {
      return Err(manager_error_response(e));
    }
  }

  if let Some(clear_on_close) = request.clear_on_close {
    if let Err(e) =
      profile_manager.update_profile_clear_on_close(&state.app_handle, &id, clear_on_close)
    {
      return Err(manager_error_response(e));
    }
  }

  // Return updated profile
  get_profile(Path(id), State(state))
    .await
    .map_err(|status| (status, String::new()))
}

#[utoipa::path(
  delete,
  path = "/v1/profiles/{id}",
  params(
    ("id" = String, Path, description = "Profile ID")
  ),
  responses(
    (status = 204, description = "Profile deleted successfully"),
    (status = 400, description = "Bad request"),
    (status = 401, description = "Unauthorized"),
    (status = 404, description = "Profile not found"),
    (status = 500, description = "Internal server error")
  ),
  security(
    ("bearer_auth" = [])
  ),
  tag = "profiles"
)]
async fn delete_profile(
  Path(id): Path<String>,
  State(state): State<ApiServerState>,
) -> Result<StatusCode, (StatusCode, String)> {
  let profile_manager = ProfileManager::instance();
  match profile_manager.delete_profile(&state.app_handle, &id) {
    Ok(_) => Ok(StatusCode::NO_CONTENT),
    Err(e) => Err(manager_error_response(e)),
  }
}

// API Handlers - Groups
#[utoipa::path(
  get,
  path = "/v1/groups",
  responses(
    (status = 200, description = "List of all groups", body = Vec<ApiGroupResponse>),
    (status = 401, description = "Unauthorized"),
    (status = 500, description = "Internal server error")
  ),
  security(
    ("bearer_auth" = [])
  ),
  tag = "groups"
)]
async fn get_groups(
  State(_state): State<ApiServerState>,
) -> Result<Json<Vec<ApiGroupResponse>>, StatusCode> {
  match GROUP_MANAGER.lock() {
    Ok(manager) => match manager.get_all_groups() {
      Ok(groups) => {
        let counts = group_profile_counts();
        let api_groups = groups
          .into_iter()
          .map(|group| ApiGroupResponse {
            profile_count: counts.get(&group.id).copied().unwrap_or(0),
            id: group.id,
            name: group.name,
          })
          .collect();
        Ok(Json(api_groups))
      }
      Err(_) => Err(StatusCode::INTERNAL_SERVER_ERROR),
    },
    Err(_) => Err(StatusCode::INTERNAL_SERVER_ERROR),
  }
}

#[utoipa::path(
  get,
  path = "/v1/groups/{id}",
  params(
    ("id" = String, Path, description = "Group ID")
  ),
  responses(
    (status = 200, description = "Group details", body = ApiGroupResponse),
    (status = 401, description = "Unauthorized"),
    (status = 404, description = "Group not found"),
    (status = 500, description = "Internal server error")
  ),
  security(
    ("bearer_auth" = [])
  ),
  tag = "groups"
)]
async fn get_group(
  Path(id): Path<String>,
  State(_state): State<ApiServerState>,
) -> Result<Json<ApiGroupResponse>, StatusCode> {
  match GROUP_MANAGER.lock() {
    Ok(manager) => match manager.get_all_groups() {
      Ok(groups) => {
        if let Some(group) = groups.into_iter().find(|g| g.id == id) {
          Ok(Json(ApiGroupResponse {
            profile_count: group_profile_counts().get(&group.id).copied().unwrap_or(0),
            id: group.id,
            name: group.name,
          }))
        } else {
          Err(StatusCode::NOT_FOUND)
        }
      }
      Err(_) => Err(StatusCode::INTERNAL_SERVER_ERROR),
    },
    Err(_) => Err(StatusCode::INTERNAL_SERVER_ERROR),
  }
}

#[utoipa::path(
  post,
  path = "/v1/groups",
  request_body = CreateGroupRequest,
  responses(
    (status = 200, description = "Group created successfully", body = ApiGroupResponse),
    (status = 400, description = "Bad request"),
    (status = 401, description = "Unauthorized"),
    (status = 500, description = "Internal server error")
  ),
  security(
    ("bearer_auth" = [])
  ),
  tag = "groups"
)]
async fn create_group(
  State(state): State<ApiServerState>,
  Json(request): Json<CreateGroupRequest>,
) -> Result<Json<ApiGroupResponse>, (StatusCode, String)> {
  match GROUP_MANAGER.lock() {
    Ok(manager) => match manager.create_group(&state.app_handle, request.name) {
      Ok(group) => Ok(Json(ApiGroupResponse {
        id: group.id,
        name: group.name,
        profile_count: 0,
      })),
      Err(e) => Err(manager_error_response(e)),
    },
    Err(_) => Err((
      StatusCode::INTERNAL_SERVER_ERROR,
      "group manager unavailable".to_string(),
    )),
  }
}

#[utoipa::path(
  put,
  path = "/v1/groups/{id}",
  params(
    ("id" = String, Path, description = "Group ID")
  ),
  request_body = UpdateGroupRequest,
  responses(
    (status = 200, description = "Group updated successfully", body = ApiGroupResponse),
    (status = 400, description = "Bad request"),
    (status = 401, description = "Unauthorized"),
    (status = 404, description = "Group not found"),
    (status = 500, description = "Internal server error")
  ),
  security(
    ("bearer_auth" = [])
  ),
  tag = "groups"
)]
async fn update_group(
  Path(id): Path<String>,
  State(state): State<ApiServerState>,
  Json(request): Json<UpdateGroupRequest>,
) -> Result<Json<ApiGroupResponse>, (StatusCode, String)> {
  match GROUP_MANAGER.lock() {
    Ok(manager) => match manager.update_group(&state.app_handle, id.clone(), request.name) {
      Ok(group) => Ok(Json(ApiGroupResponse {
        profile_count: group_profile_counts().get(&group.id).copied().unwrap_or(0),
        id: group.id,
        name: group.name,
      })),
      Err(e) => Err(manager_error_response(e)),
    },
    Err(_) => Err((
      StatusCode::INTERNAL_SERVER_ERROR,
      "group manager unavailable".to_string(),
    )),
  }
}

#[utoipa::path(
  delete,
  path = "/v1/groups/{id}",
  params(
    ("id" = String, Path, description = "Group ID")
  ),
  responses(
    (status = 204, description = "Group deleted successfully"),
    (status = 401, description = "Unauthorized"),
    (status = 404, description = "Group not found"),
    (status = 500, description = "Internal server error")
  ),
  security(
    ("bearer_auth" = [])
  ),
  tag = "groups"
)]
async fn delete_group(
  Path(id): Path<String>,
  State(state): State<ApiServerState>,
) -> Result<StatusCode, (StatusCode, String)> {
  match GROUP_MANAGER.lock() {
    Ok(manager) => match manager.delete_group(&state.app_handle, id.clone()) {
      Ok(_) => Ok(StatusCode::NO_CONTENT),
      Err(e) => Err(manager_error_response(e)),
    },
    Err(_) => Err((
      StatusCode::INTERNAL_SERVER_ERROR,
      "group manager unavailable".to_string(),
    )),
  }
}

// API Handlers - Tags
#[utoipa::path(
  get,
  path = "/v1/tags",
  responses(
    (status = 200, description = "List of all tags", body = Vec<String>),
    (status = 401, description = "Unauthorized"),
    (status = 500, description = "Internal server error")
  ),
  security(
    ("bearer_auth" = [])
  ),
  tag = "tags"
)]
async fn get_tags(State(_state): State<ApiServerState>) -> Result<Json<Vec<String>>, StatusCode> {
  match TAG_MANAGER.lock() {
    Ok(manager) => match manager.get_all_tags() {
      Ok(tags) => Ok(Json(tags)),
      Err(_) => Err(StatusCode::INTERNAL_SERVER_ERROR),
    },
    Err(_) => Err(StatusCode::INTERNAL_SERVER_ERROR),
  }
}

// API Handlers - Proxies
#[utoipa::path(
  get,
  path = "/v1/proxies",
  responses(
    (status = 200, description = "List of all proxies", body = Vec<ApiProxyResponse>),
    (status = 401, description = "Unauthorized"),
    (status = 500, description = "Internal server error")
  ),
  security(
    ("bearer_auth" = [])
  ),
  tag = "proxies"
)]
async fn get_proxies(
  State(_state): State<ApiServerState>,
) -> Result<Json<Vec<ApiProxyResponse>>, StatusCode> {
  let proxies = PROXY_MANAGER.get_stored_proxies();
  Ok(Json(
    proxies
      .into_iter()
      .map(|p| ApiProxyResponse {
        id: p.id,
        name: p.name,
        proxy_settings: p.proxy_settings,
      })
      .collect(),
  ))
}

#[utoipa::path(
  get,
  path = "/v1/proxies/{id}",
  params(
    ("id" = String, Path, description = "Proxy ID")
  ),
  responses(
    (status = 200, description = "Proxy details", body = ApiProxyResponse),
    (status = 401, description = "Unauthorized"),
    (status = 404, description = "Proxy not found"),
    (status = 500, description = "Internal server error")
  ),
  security(
    ("bearer_auth" = [])
  ),
  tag = "proxies"
)]
async fn get_proxy(
  Path(id): Path<String>,
  State(_state): State<ApiServerState>,
) -> Result<Json<ApiProxyResponse>, StatusCode> {
  let proxies = PROXY_MANAGER.get_stored_proxies();
  if let Some(proxy) = proxies.into_iter().find(|p| p.id == id) {
    Ok(Json(ApiProxyResponse {
      id: proxy.id,
      name: proxy.name,
      proxy_settings: proxy.proxy_settings,
    }))
  } else {
    Err(StatusCode::NOT_FOUND)
  }
}

#[utoipa::path(
  post,
  path = "/v1/proxies",
  request_body = CreateProxyRequest,
  responses(
    (status = 200, description = "Proxy created successfully", body = ApiProxyResponse),
    (status = 400, description = "Bad request"),
    (status = 401, description = "Unauthorized"),
    (status = 500, description = "Internal server error")
  ),
  security(
    ("bearer_auth" = [])
  ),
  tag = "proxies"
)]
async fn create_proxy(
  State(state): State<ApiServerState>,
  Json(request): Json<CreateProxyRequest>,
) -> Result<Json<ApiProxyResponse>, (StatusCode, String)> {
  let result = PROXY_MANAGER.create_stored_proxy(
    &state.app_handle,
    request.name.clone(),
    request.proxy_settings,
  );

  match result {
    Ok(proxy) => Ok(Json(ApiProxyResponse {
      id: proxy.id,
      name: proxy.name,
      proxy_settings: proxy.proxy_settings,
    })),
    Err(e) => Err(manager_error_response(e)),
  }
}

// API Handler - Bulk-import proxies from a txt list or a Duckling JSON export.
// Mirrors the MCP `import_proxies` tool.
#[utoipa::path(
  post,
  path = "/v1/proxies/import",
  request_body = ImportProxiesRequest,
  responses(
    (status = 200, description = "Import completed; inspect counts and per-proxy errors", body = ImportProxiesResponse),
    (status = 400, description = "Invalid format or no valid proxies in content"),
    (status = 401, description = "Unauthorized"),
    (status = 500, description = "Internal server error")
  ),
  security(
    ("bearer_auth" = [])
  ),
  tag = "proxies"
)]
async fn import_proxies_api(
  State(state): State<ApiServerState>,
  Json(request): Json<ImportProxiesRequest>,
) -> Result<Json<ImportProxiesResponse>, (StatusCode, String)> {
  let result = match request.format.as_str() {
    "json" => PROXY_MANAGER
      .import_proxies_json(&state.app_handle, &request.content)
      .map_err(manager_error_response)?,
    "txt" => {
      use crate::proxy_manager::{ProxyManager, ProxyParseResult};

      let parsed: Vec<_> = ProxyManager::parse_txt_proxies(&request.content)
        .into_iter()
        .filter_map(|r| match r {
          ProxyParseResult::Parsed(p) => Some(p),
          _ => None,
        })
        .collect();

      if parsed.is_empty() {
        return Err((
          StatusCode::BAD_REQUEST,
          "No valid proxies found in content".to_string(),
        ));
      }

      PROXY_MANAGER
        .import_proxies_from_parsed(&state.app_handle, parsed, request.name_prefix)
        .map_err(manager_error_response)?
    }
    other => {
      return Err((
        StatusCode::BAD_REQUEST,
        format!("Invalid format \"{other}\", must be \"json\" or \"txt\""),
      ))
    }
  };

  Ok(Json(ImportProxiesResponse {
    imported_count: result.imported_count,
    skipped_count: result.skipped_count,
    errors: result.errors,
    proxies: result
      .proxies
      .into_iter()
      .map(|p| ApiProxyResponse {
        id: p.id,
        name: p.name,
        proxy_settings: p.proxy_settings,
      })
      .collect(),
  }))
}

#[utoipa::path(
  put,
  path = "/v1/proxies/{id}",
  params(
    ("id" = String, Path, description = "Proxy ID")
  ),
  request_body = UpdateProxyRequest,
  responses(
    (status = 200, description = "Proxy updated successfully", body = ApiProxyResponse),
    (status = 400, description = "Bad request"),
    (status = 401, description = "Unauthorized"),
    (status = 404, description = "Proxy not found"),
    (status = 500, description = "Internal server error")
  ),
  security(
    ("bearer_auth" = [])
  ),
  tag = "proxies"
)]
async fn update_proxy(
  Path(id): Path<String>,
  State(state): State<ApiServerState>,
  Json(request): Json<UpdateProxyRequest>,
) -> Result<Json<ApiProxyResponse>, (StatusCode, String)> {
  let result =
    PROXY_MANAGER.update_stored_proxy(&state.app_handle, &id, request.name, request.proxy_settings);

  match result {
    Ok(proxy) => Ok(Json(ApiProxyResponse {
      id: proxy.id,
      name: proxy.name,
      proxy_settings: proxy.proxy_settings,
    })),
    Err(e) => Err(manager_error_response(e)),
  }
}

#[utoipa::path(
  delete,
  path = "/v1/proxies/{id}",
  params(
    ("id" = String, Path, description = "Proxy ID")
  ),
  responses(
    (status = 204, description = "Proxy deleted successfully"),
    (status = 400, description = "Bad request (e.g. cloud-managed proxy)"),
    (status = 401, description = "Unauthorized"),
    (status = 404, description = "Proxy not found"),
    (status = 500, description = "Internal server error")
  ),
  security(
    ("bearer_auth" = [])
  ),
  tag = "proxies"
)]
async fn delete_proxy(
  Path(id): Path<String>,
  State(state): State<ApiServerState>,
) -> Result<StatusCode, (StatusCode, String)> {
  match PROXY_MANAGER.delete_stored_proxy(&state.app_handle, &id) {
    Ok(_) => Ok(StatusCode::NO_CONTENT),
    Err(e) => Err(manager_error_response(e)),
  }
}

// API Handlers - VPNs

fn vpn_to_api_response(c: &crate::vpn::VpnConfig) -> ApiVpnResponse {
  ApiVpnResponse {
    id: c.id.clone(),
    name: c.name.clone(),
    vpn_type: c.vpn_type.to_string(),
    created_at: c.created_at,
    last_used: c.last_used,
  }
}

fn parse_vpn_type(s: &str) -> Option<crate::vpn::VpnType> {
  match s.to_ascii_lowercase().as_str() {
    "wireguard" | "wg" => Some(crate::vpn::VpnType::WireGuard),
    "vless" | "reality" | "xray" => Some(crate::vpn::VpnType::Vless),
    _ => None,
  }
}

#[utoipa::path(
  get,
  path = "/v1/vpns",
  responses(
    (status = 200, description = "List of all VPN configurations", body = Vec<ApiVpnResponse>),
    (status = 401, description = "Unauthorized"),
    (status = 500, description = "Internal server error")
  ),
  security(("bearer_auth" = [])),
  tag = "vpns"
)]
async fn get_vpns(
  State(_state): State<ApiServerState>,
) -> Result<Json<Vec<ApiVpnResponse>>, StatusCode> {
  let storage = crate::vpn::VPN_STORAGE
    .lock()
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
  let configs = storage
    .list_configs()
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
  Ok(Json(configs.iter().map(vpn_to_api_response).collect()))
}

#[utoipa::path(
  get,
  path = "/v1/vpns/{id}",
  params(("id" = String, Path, description = "VPN configuration ID")),
  responses(
    (status = 200, description = "VPN configuration details", body = ApiVpnResponse),
    (status = 401, description = "Unauthorized"),
    (status = 404, description = "VPN configuration not found"),
    (status = 500, description = "Internal server error")
  ),
  security(("bearer_auth" = [])),
  tag = "vpns"
)]
async fn get_vpn(
  Path(id): Path<String>,
  State(_state): State<ApiServerState>,
) -> Result<Json<ApiVpnResponse>, StatusCode> {
  let storage = crate::vpn::VPN_STORAGE
    .lock()
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
  let configs = storage
    .list_configs()
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
  configs
    .iter()
    .find(|c| c.id == id)
    .map(|c| Json(vpn_to_api_response(c)))
    .ok_or(StatusCode::NOT_FOUND)
}

#[utoipa::path(
  get,
  path = "/v1/vpns/{id}/export",
  params(("id" = String, Path, description = "VPN configuration ID")),
  responses(
    (status = 200, description = "Decrypted VPN configuration", body = ApiVpnExportResponse),
    (status = 401, description = "Unauthorized"),
    (status = 404, description = "VPN configuration not found"),
    (status = 500, description = "Internal server error")
  ),
  security(("bearer_auth" = [])),
  tag = "vpns"
)]
async fn export_vpn(
  Path(id): Path<String>,
  State(_state): State<ApiServerState>,
) -> Result<Json<ApiVpnExportResponse>, StatusCode> {
  let storage = crate::vpn::VPN_STORAGE
    .lock()
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
  match storage.load_config(&id) {
    Ok(config) => Ok(Json(ApiVpnExportResponse {
      id: config.id,
      name: config.name,
      vpn_type: config.vpn_type.to_string(),
      config_data: config.config_data,
    })),
    Err(_) => Err(StatusCode::NOT_FOUND),
  }
}

#[utoipa::path(
  post,
  path = "/v1/vpns/import",
  request_body = ImportVpnRequest,
  responses(
    (status = 200, description = "VPN configuration imported successfully", body = ApiVpnResponse),
    (status = 400, description = "Invalid or unrecognized VPN config"),
    (status = 401, description = "Unauthorized"),
    (status = 500, description = "Internal server error")
  ),
  security(("bearer_auth" = [])),
  tag = "vpns"
)]
async fn import_vpn(
  State(_state): State<ApiServerState>,
  Json(request): Json<ImportVpnRequest>,
) -> Result<Json<ApiVpnResponse>, StatusCode> {
  let result = {
    let storage = crate::vpn::VPN_STORAGE
      .lock()
      .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    storage.import_config(&request.content, &request.filename, request.name)
  };
  match result {
    Ok(config) => {
      let _ = events::emit("vpn-configs-changed", ());
      Ok(Json(vpn_to_api_response(&config)))
    }
    Err(_) => Err(StatusCode::BAD_REQUEST),
  }
}

#[utoipa::path(
  post,
  path = "/v1/vpns",
  request_body = CreateVpnRequest,
  responses(
    (status = 200, description = "VPN configuration created successfully", body = ApiVpnResponse),
    (status = 400, description = "Invalid VPN config or unknown vpn_type"),
    (status = 401, description = "Unauthorized"),
    (status = 500, description = "Internal server error")
  ),
  security(("bearer_auth" = [])),
  tag = "vpns"
)]
async fn create_vpn(
  State(_state): State<ApiServerState>,
  Json(request): Json<CreateVpnRequest>,
) -> Result<Json<ApiVpnResponse>, StatusCode> {
  let vpn_type = parse_vpn_type(&request.vpn_type).ok_or(StatusCode::BAD_REQUEST)?;
  let result = {
    let storage = crate::vpn::VPN_STORAGE
      .lock()
      .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    storage.create_config_manual(&request.name, vpn_type, &request.config_data)
  };
  match result {
    Ok(config) => {
      let _ = events::emit("vpn-configs-changed", ());
      Ok(Json(vpn_to_api_response(&config)))
    }
    Err(_) => Err(StatusCode::BAD_REQUEST),
  }
}

#[utoipa::path(
  put,
  path = "/v1/vpns/{id}",
  params(("id" = String, Path, description = "VPN configuration ID")),
  request_body = UpdateVpnRequest,
  responses(
    (status = 200, description = "VPN configuration updated successfully", body = ApiVpnResponse),
    (status = 400, description = "Bad request"),
    (status = 401, description = "Unauthorized"),
    (status = 404, description = "VPN configuration not found"),
    (status = 500, description = "Internal server error")
  ),
  security(("bearer_auth" = [])),
  tag = "vpns"
)]
async fn update_vpn(
  Path(id): Path<String>,
  State(_state): State<ApiServerState>,
  Json(request): Json<UpdateVpnRequest>,
) -> Result<Json<ApiVpnResponse>, StatusCode> {
  let result = {
    let storage = crate::vpn::VPN_STORAGE
      .lock()
      .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    storage.update_config_name(&id, &request.name)
  };
  match result {
    Ok(config) => {
      let _ = events::emit("vpn-configs-changed", ());
      Ok(Json(vpn_to_api_response(&config)))
    }
    Err(_) => Err(StatusCode::NOT_FOUND),
  }
}

#[utoipa::path(
  delete,
  path = "/v1/vpns/{id}",
  params(("id" = String, Path, description = "VPN configuration ID")),
  responses(
    (status = 204, description = "VPN configuration deleted successfully"),
    (status = 401, description = "Unauthorized"),
    (status = 404, description = "VPN configuration not found"),
    (status = 500, description = "Internal server error")
  ),
  security(("bearer_auth" = [])),
  tag = "vpns"
)]
async fn delete_vpn(
  Path(id): Path<String>,
  State(_state): State<ApiServerState>,
) -> Result<StatusCode, StatusCode> {
  let _ = crate::vpn_worker_runner::stop_vpn_worker_by_vpn_id(&id).await;

  let result = {
    let storage = crate::vpn::VPN_STORAGE
      .lock()
      .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    storage.delete_config(&id)
  };
  match result {
    Ok(_) => {
      let _ = events::emit("vpn-configs-changed", ());
      Ok(StatusCode::NO_CONTENT)
    }
    Err(_) => Err(StatusCode::NOT_FOUND),
  }
}

// Extension API endpoints

#[utoipa::path(
  get,
  path = "/v1/extensions",
  responses(
    (status = 200, description = "List of extensions"),
    (status = 401, description = "Unauthorized"),
  ),
  security(("bearer_auth" = [])),
  tag = "extensions"
)]
async fn get_extensions(
  State(_state): State<ApiServerState>,
) -> Result<Json<Vec<crate::extension_manager::Extension>>, StatusCode> {
  let mgr = crate::extension_manager::EXTENSION_MANAGER.lock().unwrap();
  mgr
    .list_extensions()
    .map(Json)
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

#[utoipa::path(
  get,
  path = "/v1/extension-groups",
  responses(
    (status = 200, description = "List of extension groups"),
    (status = 401, description = "Unauthorized"),
  ),
  security(("bearer_auth" = [])),
  tag = "extensions"
)]
async fn get_extension_groups(
  State(_state): State<ApiServerState>,
) -> Result<Json<Vec<crate::extension_manager::ExtensionGroup>>, StatusCode> {
  let mgr = crate::extension_manager::EXTENSION_MANAGER.lock().unwrap();
  mgr
    .list_groups()
    .map(Json)
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

#[utoipa::path(
  delete,
  path = "/v1/extensions/{id}",
  params(("id" = String, Path, description = "Extension ID")),
  responses(
    (status = 204, description = "Extension deleted"),
    (status = 401, description = "Unauthorized"),
    (status = 404, description = "Extension not found"),
    (status = 500, description = "Internal server error"),
  ),
  security(("bearer_auth" = [])),
  tag = "extensions"
)]
async fn delete_extension_api(
  Path(id): Path<String>,
  State(state): State<ApiServerState>,
) -> Result<StatusCode, (StatusCode, String)> {
  let mgr = crate::extension_manager::EXTENSION_MANAGER.lock().unwrap();
  mgr
    .delete_extension(&state.app_handle, &id)
    .map(|_| StatusCode::NO_CONTENT)
    .map_err(manager_error_response)
}

#[utoipa::path(
  delete,
  path = "/v1/extension-groups/{id}",
  params(("id" = String, Path, description = "Extension Group ID")),
  responses(
    (status = 204, description = "Extension group deleted"),
    (status = 401, description = "Unauthorized"),
    (status = 404, description = "Extension group not found"),
    (status = 500, description = "Internal server error"),
  ),
  security(("bearer_auth" = [])),
  tag = "extensions"
)]
async fn delete_extension_group_api(
  Path(id): Path<String>,
  State(state): State<ApiServerState>,
) -> Result<StatusCode, (StatusCode, String)> {
  let mgr = crate::extension_manager::EXTENSION_MANAGER.lock().unwrap();
  mgr
    .delete_group(&state.app_handle, &id)
    .map(|_| StatusCode::NO_CONTENT)
    .map_err(manager_error_response)
}

// API Handler - Run Profile with Remote Debugging
#[utoipa::path(
  post,
  path = "/v1/profiles/{id}/run",
  params(
    ("id" = String, Path, description = "Profile ID")
  ),
  request_body = RunProfileRequest,
  responses(
    (status = 200, description = "Profile launched successfully", body = RunProfileResponse),
    (status = 400, description = "Cannot launch cross-OS profile"),
    (status = 401, description = "Unauthorized"),
    (status = 404, description = "Profile not found"),
    (status = 409, description = "Profile is locked by another team member"),
    (status = 429, description = "Automation request rate limit exceeded"),
    (status = 500, description = "Internal server error")
  ),
  security(
    ("bearer_auth" = [])
  ),
  tag = "profiles"
)]
async fn run_profile(
  Path(id): Path<String>,
  State(state): State<ApiServerState>,
  Json(request): Json<RunProfileRequest>,
) -> Result<Json<RunProfileResponse>, StatusCode> {
  let headless = request.headless.unwrap_or(false);
  let url = request.url;

  let profile_manager = ProfileManager::instance();
  let profiles = profile_manager
    .list_profiles()
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

  let profile = profiles
    .iter()
    .find(|p| p.id.to_string() == id)
    .ok_or(StatusCode::NOT_FOUND)?;

  if profile.is_cross_os() {
    return Err(StatusCode::BAD_REQUEST);
  }

  // Team lock check
  crate::team_lock::acquire_team_lock_if_needed(profile)
    .await
    .map_err(|_| StatusCode::CONFLICT)?;

  let remote_debugging_port = {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
      .await
      .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let port = listener
      .local_addr()
      .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
      .port();
    drop(listener);
    port
  };

  // Use the same launch path as the main app, but force a fresh instance with
  // remote debugging enabled so the returned port is the one the browser binds.
  // Pool members fail over to the next live member on launch failure.
  match crate::proxy_pool::launch_browser_profile_with_pool_failover(
    state.app_handle.clone(),
    profile.clone(),
    url,
    Some(remote_debugging_port),
    headless,
    true,
  )
  .await
  {
    Ok(updated_profile) => Ok(Json(RunProfileResponse {
      profile_id: updated_profile.id.to_string(),
      remote_debugging_port,
      headless,
    })),
    Err(_) => Err(StatusCode::INTERNAL_SERVER_ERROR),
  }
}

// API Handler - Open URL in existing browser
#[utoipa::path(
  post,
  path = "/v1/profiles/{id}/open-url",
  params(
    ("id" = String, Path, description = "Profile ID")
  ),
  request_body = OpenUrlRequest,
  responses(
    (status = 200, description = "URL opened successfully"),
    (status = 400, description = "Cannot open URL with a cross-OS profile"),
    (status = 401, description = "Unauthorized"),
    (status = 404, description = "Profile not found"),
    (status = 429, description = "Automation request rate limit exceeded"),
    (status = 500, description = "Internal server error")
  ),
  security(
    ("bearer_auth" = [])
  ),
  tag = "profiles"
)]
async fn open_url_in_profile(
  Path(id): Path<String>,
  State(state): State<ApiServerState>,
  Json(request): Json<OpenUrlRequest>,
) -> Result<StatusCode, (StatusCode, String)> {
  let browser_runner = crate::browser_runner::BrowserRunner::instance();

  browser_runner
    .open_url_with_profile(state.app_handle.clone(), id, request.url)
    .await
    .map_err(manager_error_response)?;

  Ok(StatusCode::OK)
}

// API Handler - Kill browser process
#[utoipa::path(
  post,
  path = "/v1/profiles/{id}/kill",
  params(
    ("id" = String, Path, description = "Profile ID")
  ),
  responses(
    (status = 204, description = "Browser process killed successfully"),
    (status = 401, description = "Unauthorized"),
    (status = 404, description = "Profile not found"),
    (status = 429, description = "Automation request rate limit exceeded"),
    (status = 500, description = "Internal server error")
  ),
  security(
    ("bearer_auth" = [])
  ),
  tag = "profiles"
)]
async fn kill_profile(
  Path(id): Path<String>,
  State(state): State<ApiServerState>,
) -> Result<StatusCode, StatusCode> {
  let profile_manager = ProfileManager::instance();
  let profiles = profile_manager
    .list_profiles()
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

  let profile = profiles
    .iter()
    .find(|p| p.id.to_string() == id)
    .ok_or(StatusCode::NOT_FOUND)?;

  let browser_runner = crate::browser_runner::BrowserRunner::instance();
  browser_runner
    .kill_browser_process(state.app_handle.clone(), profile)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

  crate::team_lock::release_team_lock_if_needed(profile).await;

  Ok(StatusCode::NO_CONTENT)
}

// API Handler - Batch run profiles. Every launch parks on the global launch
// scheduler (concurrency capped by `max_concurrent_launches`), so a fleet of
// any size ramps up within that budget while results stay in request order.
#[utoipa::path(
  post,
  path = "/v1/profiles/batch/run",
  request_body = BatchRunRequest,
  responses(
    (status = 200, description = "Batch launch completed; inspect per-profile results", body = BatchRunResponse),
    (status = 401, description = "Unauthorized"),
    (status = 429, description = "Automation request rate limit exceeded"),
    (status = 500, description = "Internal server error")
  ),
  security(
    ("bearer_auth" = [])
  ),
  tag = "profiles"
)]
async fn batch_run_profiles(
  State(state): State<ApiServerState>,
  Json(request): Json<BatchRunRequest>,
) -> Result<Json<BatchRunResponse>, StatusCode> {
  let headless = request.headless.unwrap_or(false);
  let url = request.url.clone();
  let profile_manager = ProfileManager::instance();
  let profiles = profile_manager
    .list_profiles()
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

  // Every launch parks on the global launch scheduler's semaphore, which is
  // now the single concurrency cap; the stream window only needs to let all
  // queued launches be polled so the scheduler delays them, not the batching.
  let total = request.profile_ids.len().max(1);
  let results: Vec<BatchRunResult> = stream::iter(request.profile_ids)
    .map(|profile_id| {
      let profiles = profiles.clone();
      let app_handle = state.app_handle.clone();
      let url = url.clone();
      async move {
        let fail = |error: &str| BatchRunResult {
          profile_id: profile_id.clone(),
          ok: false,
          remote_debugging_port: None,
          error: Some(error.to_string()),
        };

        let Some(profile) = profiles.iter().find(|p| p.id.to_string() == *profile_id) else {
          return fail("profile not found");
        };
        if profile.is_cross_os() {
          return fail("cross-OS profiles cannot be launched");
        }
        if crate::team_lock::acquire_team_lock_if_needed(profile)
          .await
          .is_err()
        {
          return fail("profile is locked by another team member");
        }

        let port = match tokio::net::TcpListener::bind("127.0.0.1:0").await {
          Ok(listener) => match listener.local_addr() {
            Ok(addr) => addr.port(),
            Err(_) => return fail("failed to allocate debugging port"),
          },
          Err(_) => return fail("failed to allocate debugging port"),
        };

        match crate::proxy_pool::launch_browser_profile_with_pool_failover(
          app_handle,
          profile.clone(),
          url,
          Some(port),
          headless,
          true,
        )
        .await
        {
          Ok(_) => BatchRunResult {
            profile_id,
            ok: true,
            remote_debugging_port: Some(port),
            error: None,
          },
          Err(e) => fail(&format!("launch failed: {e}")),
        }
      }
    })
    .buffered(total)
    .collect()
    .await;

  Ok(Json(BatchRunResponse { results }))
}

// API Handler - Batch stop profiles.
#[utoipa::path(
  post,
  path = "/v1/profiles/batch/stop",
  request_body = BatchStopRequest,
  responses(
    (status = 200, description = "Batch stop completed; inspect per-profile results", body = BatchStopResponse),
    (status = 401, description = "Unauthorized"),
    (status = 429, description = "Automation request rate limit exceeded"),
    (status = 500, description = "Internal server error")
  ),
  security(
    ("bearer_auth" = [])
  ),
  tag = "profiles"
)]
async fn batch_stop_profiles(
  State(state): State<ApiServerState>,
  Json(request): Json<BatchStopRequest>,
) -> Result<Json<BatchStopResponse>, StatusCode> {
  let profile_manager = ProfileManager::instance();
  let profiles = profile_manager
    .list_profiles()
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
  let browser_runner = crate::browser_runner::BrowserRunner::instance();

  let results: Vec<BatchStopResult> = stream::iter(request.profile_ids)
    .map(|profile_id| {
      let profiles = profiles.clone();
      let app_handle = state.app_handle.clone();
      async move {
        let Some(profile) = profiles.iter().find(|p| p.id.to_string() == *profile_id) else {
          return BatchStopResult {
            profile_id,
            ok: false,
            error: Some("profile not found".to_string()),
          };
        };

        match browser_runner
          .kill_browser_process(app_handle, profile)
          .await
        {
          Ok(_) => {
            crate::team_lock::release_team_lock_if_needed(profile).await;
            BatchStopResult {
              profile_id,
              ok: true,
              error: None,
            }
          }
          Err(e) => BatchStopResult {
            profile_id,
            ok: false,
            error: Some(format!("stop failed: {e}")),
          },
        }
      }
    })
    .buffered(BATCH_RUN_CONCURRENCY)
    .collect()
    .await;

  Ok(Json(BatchStopResponse { results }))
}

fn pool_to_response(pool: crate::proxy_pool::ProxyPool) -> ApiProxyPoolResponse {
  ApiProxyPoolResponse {
    id: pool.id,
    name: pool.name,
    proxy_ids: pool.proxy_ids,
    updated_at: pool.updated_at,
  }
}

// API Handler - Create a proxy pool.
#[utoipa::path(
  post,
  path = "/v1/proxy-pools",
  request_body = CreateProxyPoolRequest,
  responses(
    (status = 200, description = "Proxy pool created", body = ApiProxyPoolResponse),
    (status = 400, description = "Validation error (empty/duplicate name, no members, invalid proxy)"),
    (status = 401, description = "Unauthorized"),
    (status = 500, description = "Internal server error")
  ),
  security(
    ("bearer_auth" = [])
  ),
  tag = "proxy-pools"
)]
async fn create_proxy_pool_api(
  State(_state): State<ApiServerState>,
  Json(request): Json<CreateProxyPoolRequest>,
) -> Result<Json<ApiProxyPoolResponse>, (StatusCode, String)> {
  crate::proxy_pool::PROXY_POOL_MANAGER
    .create_pool(request.name, request.proxy_ids)
    .map(pool_to_response)
    .map(Json)
    .map_err(manager_error_response)
}

// API Handler - List proxy pools.
#[utoipa::path(
  get,
  path = "/v1/proxy-pools",
  responses(
    (status = 200, description = "All proxy pools", body = Vec<ApiProxyPoolResponse>),
    (status = 401, description = "Unauthorized"),
    (status = 500, description = "Internal server error")
  ),
  security(
    ("bearer_auth" = [])
  ),
  tag = "proxy-pools"
)]
async fn list_proxy_pools_api(
  State(_state): State<ApiServerState>,
) -> Result<Json<Vec<ApiProxyPoolResponse>>, StatusCode> {
  Ok(Json(
    crate::proxy_pool::PROXY_POOL_MANAGER
      .list_pools()
      .into_iter()
      .map(pool_to_response)
      .collect(),
  ))
}

// API Handler - Update a proxy pool (name and/or members).
#[utoipa::path(
  put,
  path = "/v1/proxy-pools/{id}",
  params(
    ("id" = String, Path, description = "Proxy pool ID")
  ),
  request_body = UpdateProxyPoolRequest,
  responses(
    (status = 200, description = "Proxy pool updated", body = ApiProxyPoolResponse),
    (status = 400, description = "Validation error (empty/duplicate name, no members, invalid proxy)"),
    (status = 401, description = "Unauthorized"),
    (status = 404, description = "Proxy pool not found"),
    (status = 500, description = "Internal server error")
  ),
  security(
    ("bearer_auth" = [])
  ),
  tag = "proxy-pools"
)]
async fn update_proxy_pool_api(
  Path(id): Path<String>,
  State(_state): State<ApiServerState>,
  Json(request): Json<UpdateProxyPoolRequest>,
) -> Result<Json<ApiProxyPoolResponse>, (StatusCode, String)> {
  crate::proxy_pool::PROXY_POOL_MANAGER
    .update_pool(id, request.name, request.proxy_ids)
    .map(pool_to_response)
    .map(Json)
    .map_err(manager_error_response)
}

// API Handler - Delete a proxy pool. Profiles keep their current proxy.
#[utoipa::path(
  delete,
  path = "/v1/proxy-pools/{id}",
  params(
    ("id" = String, Path, description = "Proxy pool ID")
  ),
  responses(
    (status = 204, description = "Proxy pool deleted"),
    (status = 401, description = "Unauthorized"),
    (status = 404, description = "Proxy pool not found"),
    (status = 500, description = "Internal server error")
  ),
  security(
    ("bearer_auth" = [])
  ),
  tag = "proxy-pools"
)]
async fn delete_proxy_pool_api(
  Path(id): Path<String>,
  State(_state): State<ApiServerState>,
) -> Result<StatusCode, (StatusCode, String)> {
  crate::proxy_pool::PROXY_POOL_MANAGER
    .delete_pool(&id)
    .map(|_| StatusCode::NO_CONTENT)
    .map_err(manager_error_response)
}

// API Handler - Assign pool proxies to profiles (round-robin distribution).
#[utoipa::path(
  post,
  path = "/v1/profiles/assign-pool",
  request_body = AssignProfilesToPoolRequest,
  responses(
    (status = 200, description = "Assignment completed; inspect per-profile results", body = AssignProfilesToPoolResponse),
    (status = 400, description = "Pool not found or has no live members"),
    (status = 401, description = "Unauthorized"),
    (status = 500, description = "Internal server error")
  ),
  security(
    ("bearer_auth" = [])
  ),
  tag = "profiles"
)]
async fn assign_profiles_to_pool_api(
  State(state): State<ApiServerState>,
  Json(request): Json<AssignProfilesToPoolRequest>,
) -> Result<Json<AssignProfilesToPoolResponse>, (StatusCode, String)> {
  let results = crate::proxy_pool::PROXY_POOL_MANAGER
    .assign_profiles_to_pool(&state.app_handle, request.pool_id, request.profile_ids)
    .await
    .map_err(manager_error_response)?;
  Ok(Json(AssignProfilesToPoolResponse {
    results: results
      .into_iter()
      .map(|r| PoolAssignItem {
        profile_id: r.profile_id,
        ok: r.ok,
        proxy_id: r.proxy_id,
        error: r.error,
      })
      .collect(),
  }))
}

// API Handler - Rotate a profile to the next pool member (launch-time rotation v1).
#[utoipa::path(
  post,
  path = "/v1/profiles/{id}/rotate-proxy",
  params(
    ("id" = String, Path, description = "Profile ID")
  ),
  responses(
    (status = 200, description = "Profile rotated to the next pool member", body = RotateProfileProxyResponse),
    (status = 400, description = "Profile has no pool or the pool has a single member"),
    (status = 401, description = "Unauthorized"),
    (status = 404, description = "Profile not found"),
    (status = 500, description = "Internal server error")
  ),
  security(
    ("bearer_auth" = [])
  ),
  tag = "profiles"
)]
async fn rotate_profile_proxy_api(
  Path(id): Path<String>,
  State(state): State<ApiServerState>,
) -> Result<Json<RotateProfileProxyResponse>, (StatusCode, String)> {
  let rotated = crate::proxy_pool::PROXY_POOL_MANAGER
    .rotate_profile_proxy(&state.app_handle, &id)
    .await
    .map_err(manager_error_response)?;
  Ok(Json(RotateProfileProxyResponse {
    proxy_id: rotated.id,
    proxy_settings: rotated.proxy_settings,
  }))
}

/// Map structured LLM completion errors onto HTTP status codes:
/// validation failures are client errors, provider failures are 502.
fn llm_completion_error_response(message: String) -> (StatusCode, String) {
  let code = serde_json::from_str::<serde_json::Value>(&message)
    .ok()
    .and_then(|value| value.get("code").and_then(|c| c.as_str()).map(String::from));
  let status = match code.as_deref() {
    Some("LLM_RETRY_EXHAUSTED" | "LLM_REQUEST_FAILED") => StatusCode::BAD_GATEWAY,
    Some(_) => StatusCode::BAD_REQUEST,
    None => StatusCode::INTERNAL_SERVER_ERROR,
  };
  (status, message)
}

// API Handler - Fire a single LLM completion through the app's own key vault,
// with retry + per-provider concurrency. The key never leaves the machine.
#[utoipa::path(
  post,
  path = "/v1/llm/completion",
  request_body = ApiLlmCompletionRequest,
  responses(
    (status = 200, description = "Completion generated", body = ApiLlmCompletionResponse),
    (status = 400, description = "Validation error (empty messages, unknown provider, missing key)"),
    (status = 401, description = "Unauthorized"),
    (status = 429, description = "Hourly LLM completion quota exceeded (Retry-After header included)"),
    (status = 502, description = "Provider failed after retries"),
    (status = 500, description = "Internal server error")
  ),
  security(
    ("bearer_auth" = [])
  ),
  tag = "llm"
)]
// Large axum Response Err variant is intentional; boxing it would change the handler surface.
#[allow(clippy::result_large_err)]
async fn llm_completion_api(
  State(_state): State<ApiServerState>,
  Json(request): Json<ApiLlmCompletionRequest>,
) -> Result<Json<ApiLlmCompletionResponse>, axum::response::Response> {
  if let crate::automation_rate_limiter::RateLimitOutcome::Limited { retry_after_secs } =
    crate::llm_rate_limiter::check_llm_rate_limit()
  {
    return Err(
      (
        StatusCode::TOO_MANY_REQUESTS,
        [(header::RETRY_AFTER, retry_after_secs.to_string())],
        format!("LLM request quota exceeded; retry after {retry_after_secs}s"),
      )
        .into_response(),
    );
  }

  match crate::llm_completion::run_llm_completion(request.into()).await {
    Ok(result) => Ok(Json(result.into())),
    Err(message) => {
      let (status, body) = llm_completion_error_response(message);
      Err((status, body).into_response())
    }
  }
}

// API Handler - Detect importable browser profiles on this machine, or scan a
// custom folder. Free: importing is not gated behind browser automation.
#[utoipa::path(
  get,
  path = "/v1/profiles/import/detect",
  params(
    ("folder" = Option<String>, Query, description = "Optional folder to scan instead of the default browser locations. Accepts a single profile dir, a Chromium user-data dir, or a folder holding one profile dir per child.")
  ),
  responses(
    (status = 200, description = "Detected importable profiles", body = DetectedProfilesResponse),
    (status = 401, description = "Unauthorized"),
    (status = 404, description = "Folder not found"),
    (status = 500, description = "Internal server error")
  ),
  security(
    ("bearer_auth" = [])
  ),
  tag = "profiles"
)]
async fn detect_import_profiles(
  Query(query): Query<DetectImportQuery>,
  State(_state): State<ApiServerState>,
) -> Result<Json<DetectedProfilesResponse>, (StatusCode, String)> {
  let importer = crate::profile_importer::ProfileImporter::instance();
  let profiles = match query.folder.as_deref() {
    Some(folder) => importer.scan_folder(std::path::Path::new(folder)),
    None => importer.detect_existing_profiles(),
  }
  .map_err(manager_error_response)?;
  let total = profiles.len();
  Ok(Json(DetectedProfilesResponse { profiles, total }))
}

#[utoipa::path(
  get,
  path = "/v1/system/status",
  responses(
    (status = 200, description = "Global scheduler and launch-pipeline metrics", body = crate::launch_scheduler::LaunchSchedulerMetrics),
    (status = 401, description = "Unauthorized"),
    (status = 500, description = "Internal server error")
  ),
  security(
    ("bearer_auth" = [])
  ),
  tag = "system"
)]
// Large axum Response Err variant is intentional; boxing it would change the handler surface.
#[allow(clippy::result_large_err)]
async fn system_status_api(
  State(_state): State<ApiServerState>,
) -> Result<Json<crate::launch_scheduler::LaunchSchedulerMetrics>, axum::response::Response> {
  Ok(Json(
    crate::launch_scheduler::LaunchScheduler::instance().metrics(),
  ))
}

// API Handler - Bulk-import browser profiles from on-disk profile folders.
// Free (parity with create_profile); only fingerprint OS spoofing is Pro.
// Items are isolated — one failure doesn't stop the rest.
#[utoipa::path(
  post,
  path = "/v1/profiles/import",
  request_body = ImportProfilesRequest,
  responses(
    (status = 200, description = "Batch import completed; inspect per-item results", body = crate::profile_importer::ProfileImportBatchResult),
    (status = 400, description = "No items, or invalid input"),
    (status = 401, description = "Unauthorized"),
    (status = 402, description = "Fingerprint OS spoofing requires an active Pro subscription"),
    (status = 404, description = "Group not found"),
    (status = 500, description = "Internal server error")
  ),
  security(
    ("bearer_auth" = [])
  ),
  tag = "profiles"
)]
async fn import_profiles_api(
  State(state): State<ApiServerState>,
  Json(request): Json<ImportProfilesRequest>,
) -> Result<Json<crate::profile_importer::ProfileImportBatchResult>, (StatusCode, String)> {
  let chromium_config: Option<crate::chromium_manager::ChromiumConfig> = request
    .chromium_config
    .as_ref()
    .and_then(|config| serde_json::from_value(config.clone()).ok());

  // The Pro gate for fingerprint OS spoofing lives inside import_profiles, so
  // every surface inherits it; manager_error_response maps the code to 402.
  let importer = crate::profile_importer::ProfileImporter::instance();
  importer
    .import_profiles(
      &state.app_handle,
      request.items,
      request.group_id,
      request.duplicate_strategy.unwrap_or_default(),
      chromium_config,
    )
    .await
    .map(Json)
    .map_err(manager_error_response)
}

#[utoipa::path(
  post,
  path = "/v1/profiles/{id}/cookies/import",
  params(
    ("id" = String, Path, description = "Profile ID")
  ),
  request_body = ImportCookiesRequest,
  responses(
    (status = 200, description = "Cookies imported successfully", body = ImportCookiesResponse),
    (status = 400, description = "Invalid cookie file or unsupported browser"),
    (status = 401, description = "Unauthorized"),
    (status = 404, description = "Profile not found"),
    (status = 409, description = "Browser is currently running"),
    (status = 500, description = "Internal server error")
  ),
  security(
    ("bearer_auth" = [])
  ),
  tag = "cookies"
)]
async fn import_profile_cookies(
  Path(id): Path<String>,
  State(state): State<ApiServerState>,
  Json(request): Json<ImportCookiesRequest>,
) -> Result<Json<ImportCookiesResponse>, StatusCode> {
  let profile_manager = ProfileManager::instance();
  let profiles = profile_manager
    .list_profiles()
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

  if !profiles.iter().any(|p| p.id.to_string() == id) {
    return Err(StatusCode::NOT_FOUND);
  }

  match crate::cookie_manager::CookieManager::import_cookies(
    &state.app_handle,
    &id,
    &request.content,
  )
  .await
  {
    Ok(result) => {
      if let Some(scheduler) = crate::sync::get_global_scheduler() {
        if let Some(profile) = profiles.iter().find(|p| p.id.to_string() == id) {
          if profile.is_sync_enabled() {
            let pid = id.clone();
            tauri::async_runtime::spawn(async move {
              scheduler.queue_profile_sync(pid).await;
            });
          }
        }
      }
      Ok(Json(ImportCookiesResponse {
        cookies_imported: result.cookies_imported,
        cookies_replaced: result.cookies_replaced,
        errors: result.errors,
      }))
    }
    Err(e) => {
      let msg = e.to_lowercase();
      if msg.contains("running") {
        Err(StatusCode::CONFLICT)
      } else if msg.contains("no valid cookies") || msg.contains("unsupported browser") {
        Err(StatusCode::BAD_REQUEST)
      } else {
        Err(StatusCode::INTERNAL_SERVER_ERROR)
      }
    }
  }
}

// API Handler - Download Browser
#[utoipa::path(
  post,
  path = "/v1/browsers/download",
  request_body = DownloadBrowserRequest,
  responses(
    (status = 200, description = "Browser downloaded (or already present)", body = DownloadBrowserResponse),
    (status = 400, description = "Invalid browser or version not available for download"),
    (status = 401, description = "Unauthorized"),
    (status = 409, description = "This browser version is already being downloaded"),
    (status = 500, description = "Internal server error (e.g. network failure)")
  ),
  security(
    ("bearer_auth" = [])
  ),
  tag = "browsers"
)]
async fn download_browser_api(
  State(state): State<ApiServerState>,
  Json(request): Json<DownloadBrowserRequest>,
) -> Result<Json<DownloadBrowserResponse>, (StatusCode, String)> {
  match crate::downloader::download_browser(
    state.app_handle.clone(),
    request.browser.clone(),
    request.version,
  )
  .await
  {
    // Echo the version the downloader actually installed, not the requested one.
    Ok(version) => Ok(Json(DownloadBrowserResponse {
      browser: request.browser,
      version,
      status: "downloaded".to_string(),
    })),
    Err(e) => {
      if e.contains("already being downloaded") {
        Err((StatusCode::CONFLICT, e))
      } else {
        Err(manager_error_response(e))
      }
    }
  }
}

// API Handler - Get Browser Versions
#[utoipa::path(
  get,
  path = "/v1/browsers/{browser}/versions",
  params(
    ("browser" = String, Path, description = "Browser name")
  ),
  responses(
    (status = 200, description = "List of available browser versions", body = Vec<String>),
    (status = 400, description = "Unsupported browser"),
    (status = 401, description = "Unauthorized"),
    (status = 500, description = "Internal server error")
  ),
  security(
    ("bearer_auth" = [])
  ),
  tag = "browsers"
)]
async fn get_browser_versions(
  Path(browser): Path<String>,
  State(_state): State<ApiServerState>,
) -> Result<Json<Vec<String>>, (StatusCode, String)> {
  let version_manager = crate::browser_version_manager::BrowserVersionManager::instance();

  match version_manager
    .fetch_browser_versions_with_count(&browser, false)
    .await
  {
    Ok(result) => Ok(Json(result.versions)),
    Err(e) => Err(manager_error_response(e)),
  }
}

// API Handler - Check if Browser is Downloaded
#[utoipa::path(
  get,
  path = "/v1/browsers/{browser}/versions/{version}/downloaded",
  params(
    ("browser" = String, Path, description = "Browser name"),
    ("version" = String, Path, description = "Browser version")
  ),
  responses(
    (status = 200, description = "Browser download status", body = bool),
    (status = 401, description = "Unauthorized"),
    (status = 500, description = "Internal server error")
  ),
  security(
    ("bearer_auth" = [])
  ),
  tag = "browsers"
)]
async fn check_browser_downloaded(
  Path((browser, version)): Path<(String, String)>,
  State(_state): State<ApiServerState>,
) -> Result<Json<bool>, StatusCode> {
  let is_downloaded = crate::downloaded_browsers_registry::is_browser_downloaded(browser, version);
  Ok(Json(is_downloaded))
}

#[cfg(test)]
mod tests {
  use super::*;

  // Removing `browser` from UpdateProfileRequest, and rejecting invalid
  // `browser` values on create, must NOT make the API reject requests that
  // carry extra/unknown fields — old clients still send them. serde ignores
  // unknown fields by default; these tests lock that in so a future
  // `#[serde(deny_unknown_fields)]` can't silently break compatibility.
  #[test]
  fn update_profile_request_ignores_unknown_fields() {
    // `browser` is no longer a field, plus a wholly unknown field. Both must
    // be accepted and ignored, not rejected.
    let json = r#"{"name": "p", "browser": "chromium", "totally_unknown": 123}"#;
    let parsed: UpdateProfileRequest =
      serde_json::from_str(json).expect("unknown fields must be ignored, not rejected");
    assert_eq!(parsed.name.as_deref(), Some("p"));
  }

  #[test]
  fn create_profile_request_ignores_unknown_fields() {
    let json = r#"{"name": "p", "browser": "chromium", "version": "latest", "future_field": true}"#;
    let parsed: CreateProfileRequest =
      serde_json::from_str(json).expect("unknown fields must be ignored, not rejected");
    assert_eq!(parsed.browser, "chromium");
  }

  #[test]
  fn create_profile_request_allows_omitting_version_and_configs() {
    // Minimal body: no version, no chromium_config. Must
    // deserialize (version resolves to latest-downloaded at the handler; an
    // absent config triggers fresh-fingerprint generation).
    let json = r#"{"name": "p", "browser": "chromium"}"#;
    let parsed: CreateProfileRequest =
      serde_json::from_str(json).expect("version and configs are optional");
    assert_eq!(parsed.browser, "chromium");
    assert!(parsed.version.is_none());
    assert!(parsed.chromium_config.is_none());
  }

  #[test]
  fn create_profile_browser_validation_matches_supported_engines() {
    // The handler rejects anything that isn't a launchable engine; this is the
    // same predicate it uses, kept in lockstep with MCP's create_profile.
    let is_valid = |b: &str| b == "chromium";
    assert!(is_valid("chromium"));
    assert!(!is_valid("firefox"));
    assert!(!is_valid(""));
  }

  #[test]
  fn rate_limit_only_classifies_browser_automation_routes() {
    for path in [
      "/v1/profiles/profile-id/run",
      "/v1/profiles/profile-id/open-url",
      "/v1/profiles/profile-id/kill",
      "/v1/profiles/batch/run",
      "/v1/profiles/batch/stop",
    ] {
      assert!(
        is_automation_request(&Method::POST, path),
        "automation route was not limited: {path}"
      );
    }

    for (method, path) in [
      (Method::GET, "/v1/profiles/profile-id/run"),
      (Method::POST, "/v1/profiles"),
      (Method::POST, "/v1/profiles/import"),
      (Method::POST, "/v1/profiles/assign-pool"),
      (Method::POST, "/v1/profiles/profile-id/rotate-proxy"),
      (Method::POST, "/v1/proxy-pools"),
      (Method::GET, "/v1/proxy-pools"),
      (Method::PUT, "/v1/proxy-pools/pool-id"),
      (Method::DELETE, "/v1/proxy-pools/pool-id"),
      (Method::GET, "/v1/profiles"),
      (Method::GET, "/openapi.json"),
    ] {
      assert!(
        !is_automation_request(&method, path),
        "free or non-mutating route was limited: {method} {path}"
      );
    }
  }

  fn schema_required(spec: &serde_json::Value, schema: &str) -> Vec<String> {
    spec["components"]["schemas"][schema]["required"]
      .as_array()
      .map(|a| {
        a.iter()
          .filter_map(|v| v.as_str().map(str::to_string))
          .collect()
      })
      .unwrap_or_default()
  }

  // `#[schema(value_type = Object)]` on an `Option<T>` erases the optionality
  // and marks the field required in the served spec; these fields must stay
  // optional so generated clients aren't forced to send them.
  #[test]
  fn openapi_optional_fields_are_not_required() {
    let spec = serde_json::to_value(ApiDoc::openapi()).expect("spec serializes");

    let create_profile = schema_required(&spec, "CreateProfileRequest");
    assert!(
      !create_profile.iter().any(|f| f == "chromium_config"),
      "chromium_config must be optional, required list: {create_profile:?}"
    );

    let update_profile = schema_required(&spec, "UpdateProfileRequest");
    assert!(
      !update_profile.iter().any(|f| f == "group_id"),
      "group_id must be optional, required list: {update_profile:?}"
    );

    let update_proxy = schema_required(&spec, "UpdateProxyRequest");
    assert!(
      !update_proxy.iter().any(|f| f == "proxy_settings"),
      "proxy_settings must be optional on update, required list: {update_proxy:?}"
    );

    let import_profiles = schema_required(&spec, "ImportProfilesRequest");
    for field in ["group_id", "duplicate_strategy", "chromium_config"] {
      assert!(
        !import_profiles.iter().any(|f| f == field),
        "{field} must be optional on profile import, required list: {import_profiles:?}"
      );
    }

    let import_item = schema_required(&spec, "ImportProfileItem");
    for field in ["proxy_id", "vpn_id", "browser_type"] {
      assert!(
        !import_item.iter().any(|f| f == field),
        "{field} must be optional on import items, required list: {import_item:?}"
      );
    }
  }

  #[test]
  fn import_profiles_request_allows_minimal_body() {
    // Only items with source_path + new_profile_name are required; everything
    // else has defaults.
    let json = r#"{"items": [{"source_path": "/tmp/p", "new_profile_name": "Imported"}]}"#;
    let parsed: ImportProfilesRequest =
      serde_json::from_str(json).expect("minimal import body must deserialize");
    assert_eq!(parsed.items.len(), 1);
    assert!(parsed.group_id.is_none());
    assert!(parsed.duplicate_strategy.is_none());
    assert_eq!(parsed.items[0].browser_type, "chromium");
  }

  // The served /openapi.json comes from the hand-maintained ApiDoc `paths(...)`
  // list, not from the router — endpoints registered on the router but missing
  // from ApiDoc silently disappear from the spec. Lock in the ones that were
  // once dropped, and that removed endpoints stay gone.
  #[test]
  fn openapi_spec_covers_registered_routes() {
    let spec = serde_json::to_value(ApiDoc::openapi()).expect("spec serializes");
    let paths = spec["paths"].as_object().expect("paths object");

    for path in [
      "/v1/vpns/{id}/export",
      "/v1/extensions",
      "/v1/extension-groups",
      "/v1/extensions/{id}",
      "/v1/extension-groups/{id}",
      "/v1/profiles/import",
      "/v1/profiles/import/detect",
      "/v1/proxies/import",
      "/v1/proxy-pools",
      "/v1/proxy-pools/{id}",
      "/v1/profiles/assign-pool",
      "/v1/profiles/{id}/rotate-proxy",
      "/v1/system/status",
    ] {
      assert!(paths.contains_key(path), "missing from ApiDoc: {path}");
    }

    assert!(
      !paths.keys().any(|p| p.contains("Chromium-token")),
      "Chromium-token endpoints were removed and must stay out of the spec"
    );

    for path in [
      "/v1/profiles/{id}/run",
      "/v1/profiles/{id}/open-url",
      "/v1/profiles/{id}/kill",
      "/v1/profiles/batch/run",
      "/v1/profiles/batch/stop",
    ] {
      assert!(
        paths[path]["post"]["responses"].get("429").is_some(),
        "automation route is missing its 429 response: {path}"
      );
    }
  }

  // VLESS + Reality are first-class VPN types alongside WireGuard; the REST API
  // must accept them on create and surface `vpn_type` on responses/exports.
  #[test]
  fn parse_vpn_type_accepts_wireguard_and_vless() {
    assert!(matches!(
      parse_vpn_type("WireGuard"),
      Some(crate::vpn::VpnType::WireGuard)
    ));
    assert!(matches!(
      parse_vpn_type("wg"),
      Some(crate::vpn::VpnType::WireGuard)
    ));
    assert!(matches!(
      parse_vpn_type("Vless"),
      Some(crate::vpn::VpnType::Vless)
    ));
    assert!(matches!(
      parse_vpn_type("vless"),
      Some(crate::vpn::VpnType::Vless)
    ));
    assert!(matches!(
      parse_vpn_type("reality"),
      Some(crate::vpn::VpnType::Vless)
    ));
    assert!(matches!(
      parse_vpn_type("xray"),
      Some(crate::vpn::VpnType::Vless)
    ));
    assert!(parse_vpn_type("openvpn").is_none());
    assert!(parse_vpn_type("").is_none());
  }

  #[test]
  fn openapi_vpn_schemas_surface_vpn_type() {
    let spec = serde_json::to_value(ApiDoc::openapi()).expect("spec serializes");
    for schema in ["ApiVpnResponse", "ApiVpnExportResponse", "CreateVpnRequest"] {
      let required = schema_required(&spec, schema);
      assert!(
        required.iter().any(|f| f == "vpn_type"),
        "{schema} must require vpn_type so callers cannot omit it, required: {required:?}"
      );
    }
    // The served ApiVpnResponse description should no longer claim "Always WireGuard".
    let desc = spec["components"]["schemas"]["ApiVpnResponse"]["properties"]["vpn_type"]
      ["description"]
      .as_str()
      .unwrap_or("");
    assert!(
      !desc.contains("Always"),
      "ApiVpnResponse.vpn_type description must be generic, got: {desc}"
    );
  }

  #[test]
  fn batch_create_request_allows_minimal_body() {
    let json = r#"{"names": ["A", "B"], "browser": "chromium"}"#;
    let parsed: BatchCreateProfilesRequest =
      serde_json::from_str(json).expect("minimal batch body must deserialize");
    assert_eq!(parsed.names, vec!["A", "B"]);
    assert!(parsed.version.is_none());
    assert!(parsed.chromium_config.is_none());
    assert!(!parsed.ephemeral);
  }

  #[test]
  fn batch_create_request_supports_prefix_and_count() {
    let json =
      r#"{"name_prefix": "Account", "count": 5, "browser": "chromium", "ephemeral": true}"#;
    let parsed: BatchCreateProfilesRequest =
      serde_json::from_str(json).expect("prefix/count body must deserialize");
    assert!(parsed.names.is_empty());
    assert_eq!(parsed.name_prefix.as_deref(), Some("Account"));
    assert_eq!(parsed.count, Some(5));
    assert!(parsed.ephemeral);
  }

  #[test]
  fn batch_create_is_not_automation_limited() {
    // Creation is free (like POST /v1/profiles) — it must never consume the
    // automation quota, or mass provisioning would 429 instantly.
    for method in [Method::POST, Method::GET] {
      assert!(
        !is_automation_request(&method, "/v1/profiles/batch/create"),
        "batch create must not be automation-limited"
      );
    }
  }

  #[test]
  fn openapi_spec_covers_batch_create() {
    let spec = serde_json::to_value(ApiDoc::openapi()).expect("spec serializes");
    let paths = spec["paths"].as_object().expect("paths object");
    assert!(
      paths.contains_key("/v1/profiles/batch/create"),
      "batch create endpoint missing from ApiDoc"
    );
    let create = &paths["/v1/profiles/batch/create"]["post"];
    assert!(
      create["responses"].get("200").is_some(),
      "batch create must document its 200 response"
    );

    let required = schema_required(&spec, "BatchCreateProfilesRequest");
    for field in [
      "names",
      "name_prefix",
      "count",
      "version",
      "release_type",
      "chromium_config",
      "proxy_id",
      "vpn_id",
      "group_id",
      "ephemeral",
      "dns_blocklist",
      "launch_hook",
    ] {
      assert!(
        !required.iter().any(|f| f == field),
        "{field} must be optional on batch create, required: {required:?}"
      );
    }
  }
}
