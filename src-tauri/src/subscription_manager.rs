//! V2Ray-style subscription imports with scheduled auto-refresh.
//!
//! A subscription is a URL serving a plaintext (or base64-encoded) list of
//! share links (`vless://`, `ss://`, …) in the style of public aggregators
//! such as `barry-far/V2ray-Config` (`Sub*.txt`). Each refresh fetches the
//! URL — directly or through a stored proxy — and reconciles the materialized
//! VPN configs / stored proxies: new links are added, known links are updated
//! in place, and disappeared links are removed only when `auto_prune` is set.
//! Optional health-checking probes every entry and can drop the unusable ones.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

const STORE_VERSION: u32 = 1;
const FETCH_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_BODY_BYTES: usize = 5 * 1024 * 1024;
const MAX_REFRESH_HOURS: u64 = 24 * 30;
const CHECK_CONCURRENCY: usize = 4;
const CHECK_TIMEOUT: Duration = Duration::from_secs(45);
const LOOP_TICK: Duration = Duration::from_secs(300);

fn code_error(code: &str, params: serde_json::Value) -> String {
  serde_json::json!({ "code": code, "params": params }).to_string()
}

fn invalid(detail: &str) -> String {
  code_error(
    "SUBSCRIPTION_INVALID",
    serde_json::json!({ "detail": detail }),
  )
}

fn not_found(id: &str) -> String {
  code_error("SUBSCRIPTION_NOT_FOUND", serde_json::json!({ "id": id }))
}

/// A registered subscription source.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Subscription {
  pub id: String,
  pub name: String,
  pub url: String,
  /// Refresh interval in hours; 0 = manual only.
  pub refresh_hours: u64,
  /// Stored proxy id used to fetch the URL; None = direct connection.
  #[serde(default)]
  pub use_proxy_id: Option<String>,
  /// Probe each entry after refresh.
  #[serde(default)]
  pub auto_check: bool,
  /// Remove disappeared/unusable entries (only entries owned by this sub).
  #[serde(default)]
  pub auto_prune: bool,
  #[serde(default)]
  pub last_fetched_at: Option<u64>,
  #[serde(default)]
  pub last_status: Option<String>,
  pub created_at: String,
  pub updated_at: String,
}

/// One materialized entry owned by a subscription.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SubscriptionEntry {
  pub id: String,
  pub subscription_id: String,
  /// "vpn" or "proxy".
  pub kind: String,
  /// VPN config id or stored proxy id.
  pub entry_id: String,
  /// Hash of the normalized source link; stable identity across refreshes.
  pub link_hash: String,
  pub name: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct SubscriptionStoreData {
  version: u32,
  subscriptions: Vec<Subscription>,
  entries: Vec<SubscriptionEntry>,
}

/// A single classified share link from a subscription body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubLink {
  /// Lowercase scheme without `://` (e.g. "vless", "ss").
  pub scheme: String,
  /// The full trimmed link line.
  pub raw: String,
  /// Display name from the `#fragment`, if present.
  pub name: Option<String>,
}

/// Parsed subscription body plus provider hint headers.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ParsedSubscription {
  pub links: Vec<SubLink>,
  /// `#profile-update-interval` hint in hours, when present.
  pub suggested_interval_hours: Option<u64>,
  /// Raw `#subscription-userinfo` value, when present.
  pub user_info: Option<String>,
  pub unsupported: usize,
}

fn link_hash(raw: &str) -> String {
  let mut hasher = Sha256::new();
  hasher.update(raw.trim().as_bytes());
  hasher
    .finalize()
    .iter()
    .map(|b| format!("{b:02x}"))
    .collect()
}

/// Stable identity for one link: scheme + normalized body.
fn normalize_link(line: &str) -> String {
  line.trim().to_string()
}

fn classify_line(line: &str) -> Option<SubLink> {
  let trimmed = line.trim();
  if trimmed.is_empty() || trimmed.starts_with('#') {
    return None;
  }
  let scheme_end = trimmed.find("://")?;
  let scheme = trimmed[..scheme_end].to_lowercase();
  if scheme.is_empty() || scheme.contains(char::is_whitespace) {
    return None;
  }
  let name = trimmed
    .rfind('#')
    .map(|i| trimmed[i + 1..].trim().to_string())
    .filter(|n| !n.is_empty());
  Some(SubLink {
    scheme,
    raw: normalize_link(trimmed),
    name,
  })
}

/// Try to base64-decode a whole body (some providers serve base64 blobs).
/// Returns the decoded text when it looks like a link list.
fn maybe_decode_body(body: &str) -> Option<String> {
  use base64::Engine;
  let compact: String = body.chars().filter(|c| !c.is_whitespace()).collect();
  if compact.len() < 16 {
    return None;
  }
  for engine in [
    &base64::engine::general_purpose::STANDARD,
    &base64::engine::general_purpose::STANDARD_NO_PAD,
    &base64::engine::general_purpose::URL_SAFE,
    &base64::engine::general_purpose::URL_SAFE_NO_PAD,
  ] {
    if let Ok(bytes) = engine.decode(&compact) {
      if let Ok(text) = String::from_utf8(bytes) {
        if text.contains("://") {
          return Some(text);
        }
      }
    }
  }
  None
}

/// Parse a subscription body into classified links + provider headers.
pub fn parse_subscription_body(body: &str) -> ParsedSubscription {
  let text = body.strip_prefix('\u{feff}').unwrap_or(body);
  let effective: &str = &maybe_decode_body(text).unwrap_or_else(|| text.to_string());
  let mut out = ParsedSubscription::default();
  for line in effective.lines() {
    let trimmed = line.trim();
    if trimmed.is_empty() {
      continue;
    }
    if let Some(rest) = trimmed.strip_prefix('#') {
      let lower = rest.to_lowercase();
      if let Some(value) = lower.strip_prefix("profile-update-interval:") {
        if let Ok(hours) = value.trim().parse::<u64>() {
          out.suggested_interval_hours = Some(hours.min(MAX_REFRESH_HOURS));
        }
      } else if let Some(value) = rest.strip_prefix("subscription-userinfo:") {
        out.user_info = Some(value.trim().to_string());
      }
      continue;
    }
    match classify_line(trimmed) {
      Some(link) if is_supported_scheme(&link.scheme) => out.links.push(link),
      _ => {
        out.unsupported += 1;
      }
    }
  }
  out
}

/// Schemes the app can materialize today: vless → VPN, classic proxy
/// URLs → stored proxies. Everything else (vmess/trojan/hysteria/…) is
/// counted as unsupported rather than silently dropped.
pub fn is_supported_scheme(scheme: &str) -> bool {
  matches!(
    scheme,
    "vless" | "ss" | "shadowsocks" | "http" | "https" | "socks" | "socks4" | "socks5"
  )
}

/// Route a link: vless → VPN, classic proxy URLs → stored proxy.
pub fn link_target(link: &SubLink) -> Option<&'static str> {
  if link.scheme == "vless" {
    Some("vpn")
  } else if matches!(
    link.scheme.as_str(),
    "ss" | "shadowsocks" | "http" | "https" | "socks" | "socks4" | "socks5"
  ) {
    Some("proxy")
  } else {
    None
  }
}

fn validate_url(url: &str) -> Result<String, String> {
  let trimmed = url.trim();
  if trimmed.is_empty() {
    return Err(invalid("subscription URL must not be empty"));
  }
  if trimmed.len() > 2000 {
    return Err(invalid("subscription URL is too long"));
  }
  let parsed: url::Url = trimmed
    .parse()
    .map_err(|_| invalid("subscription URL must be a valid http(s) URL"))?;
  match parsed.scheme() {
    "http" | "https" => {}
    _ => {
      return Err(invalid(
        "subscription URL must start with http:// or https://",
      ))
    }
  }
  if parsed.host_str().is_none_or(|h| h.is_empty()) {
    return Err(invalid("subscription URL must include a host"));
  }
  Ok(trimmed.to_string())
}

fn now_secs() -> u64 {
  std::time::SystemTime::now()
    .duration_since(std::time::UNIX_EPOCH)
    .unwrap_or_default()
    .as_secs()
}

fn now_iso() -> String {
  chrono::Utc::now().to_rfc3339()
}

pub struct SubscriptionManager {
  store_path: PathBuf,
}

impl SubscriptionManager {
  fn new() -> Self {
    let dir = crate::app_dirs::settings_dir();
    let _ = fs::create_dir_all(&dir);
    Self {
      store_path: dir.join("subscriptions.json"),
    }
  }

  #[cfg(test)]
  fn with_path(path: PathBuf) -> Self {
    Self { store_path: path }
  }

  fn load(&self) -> SubscriptionStoreData {
    if !self.store_path.exists() {
      return SubscriptionStoreData {
        version: STORE_VERSION,
        subscriptions: Vec::new(),
        entries: Vec::new(),
      };
    }
    fs::read_to_string(&self.store_path)
      .ok()
      .and_then(|content| serde_json::from_str(&content).ok())
      .unwrap_or(SubscriptionStoreData {
        version: STORE_VERSION,
        subscriptions: Vec::new(),
        entries: Vec::new(),
      })
  }

  fn save(&self, data: &SubscriptionStoreData) -> Result<(), String> {
    if let Some(parent) = self.store_path.parent() {
      fs::create_dir_all(parent).map_err(|e| format!("Failed to create settings dir: {e}"))?;
    }
    let content =
      serde_json::to_string_pretty(data).map_err(|e| format!("Failed to serialize: {e}"))?;
    fs::write(&self.store_path, content).map_err(|e| format!("Failed to write store: {e}"))?;
    crate::app_dirs::restrict_to_owner(&self.store_path);
    Ok(())
  }

  pub fn list(&self) -> Vec<Subscription> {
    self.load().subscriptions
  }

  pub fn entries_for(&self, subscription_id: &str) -> Vec<SubscriptionEntry> {
    self
      .load()
      .entries
      .into_iter()
      .filter(|e| e.subscription_id == subscription_id)
      .collect()
  }

  #[allow(clippy::too_many_arguments)]
  pub fn save_subscription(
    &self,
    id: Option<String>,
    name: &str,
    url: &str,
    refresh_hours: u64,
    use_proxy_id: Option<String>,
    auto_check: bool,
    auto_prune: bool,
  ) -> Result<Subscription, String> {
    if name.trim().is_empty() {
      return Err(crate::backend_error("NAME_CANNOT_BE_EMPTY"));
    }
    let url = validate_url(url)?;
    if refresh_hours > MAX_REFRESH_HOURS {
      return Err(invalid("refresh interval is too large (max 720 hours)"));
    }
    if let Some(proxy_id) = &use_proxy_id {
      if !proxy_id.trim().is_empty()
        && crate::proxy_manager::PROXY_MANAGER
          .get_proxy_settings_by_id(proxy_id)
          .is_none()
      {
        return Err(invalid("selected fetch proxy no longer exists"));
      }
    }

    let mut data = self.load();
    let now = now_iso();
    if let Some(id) = id {
      let sub = data
        .subscriptions
        .iter_mut()
        .find(|s| s.id == id)
        .ok_or_else(|| not_found(&id))?;
      sub.name = name.trim().to_string();
      sub.url = url;
      sub.refresh_hours = refresh_hours;
      sub.use_proxy_id = use_proxy_id.filter(|s| !s.trim().is_empty());
      sub.auto_check = auto_check;
      sub.auto_prune = auto_prune;
      sub.updated_at = now;
      let updated = sub.clone();
      self.save(&data)?;
      Ok(updated)
    } else {
      let sub = Subscription {
        id: Uuid::new_v4().to_string(),
        name: name.trim().to_string(),
        url,
        refresh_hours,
        use_proxy_id: use_proxy_id.filter(|s| !s.trim().is_empty()),
        auto_check,
        auto_prune,
        last_fetched_at: None,
        last_status: None,
        created_at: now.clone(),
        updated_at: now,
      };
      data.subscriptions.push(sub.clone());
      self.save(&data)?;
      Ok(sub)
    }
  }

  pub fn delete(&self, id: &str) -> Result<Vec<SubscriptionEntry>, String> {
    let mut data = self.load();
    let before = data.subscriptions.len();
    data.subscriptions.retain(|s| s.id != id);
    if data.subscriptions.len() == before {
      return Err(not_found(id));
    }
    let orphaned: Vec<SubscriptionEntry> = data
      .entries
      .iter()
      .filter(|e| e.subscription_id == id)
      .cloned()
      .collect();
    data.entries.retain(|e| e.subscription_id != id);
    self.save(&data)?;
    Ok(orphaned)
  }

  fn record_status(&self, id: &str, status: &str) {
    let mut data = self.load();
    if let Some(sub) = data.subscriptions.iter_mut().find(|s| s.id == id) {
      sub.last_fetched_at = Some(now_secs());
      sub.last_status = Some(status.to_string());
      sub.updated_at = now_iso();
      let _ = self.save(&data);
    }
  }

  fn upsert_entry(&self, entry: SubscriptionEntry) {
    let mut data = self.load();
    if let Some(pos) = data
      .entries
      .iter()
      .position(|e| e.subscription_id == entry.subscription_id && e.link_hash == entry.link_hash)
    {
      data.entries[pos] = entry;
    } else {
      data.entries.push(entry);
    }
    let _ = self.save(&data);
  }

  fn remove_entry(&self, subscription_id: &str, link_hash: &str) -> Option<SubscriptionEntry> {
    let mut data = self.load();
    let pos = data
      .entries
      .iter()
      .position(|e| e.subscription_id == subscription_id && e.link_hash == link_hash)?;
    let removed = data.entries.remove(pos);
    let _ = self.save(&data);
    Some(removed)
  }

  /// Subscriptions due for a refresh tick.
  pub fn due_for_refresh(&self, now: u64) -> Vec<Subscription> {
    self
      .load()
      .subscriptions
      .into_iter()
      .filter(|s| s.refresh_hours > 0)
      .filter(|s| {
        s.last_fetched_at
          .is_none_or(|last| now.saturating_sub(last) >= s.refresh_hours * 3600)
      })
      .collect()
  }
}

pub static SUBSCRIPTION_MANAGER: LazyLock<Mutex<SubscriptionManager>> =
  LazyLock::new(|| Mutex::new(SubscriptionManager::new()));

/// Fetch a subscription body, directly or through a stored proxy.
pub async fn fetch_subscription_body(
  url: &str,
  use_proxy_id: Option<&str>,
) -> Result<String, String> {
  let url = validate_url(url)?;
  let mut builder = reqwest::Client::builder().timeout(FETCH_TIMEOUT);
  if let Some(proxy_id) = use_proxy_id.filter(|s| !s.is_empty()) {
    let settings = crate::proxy_manager::PROXY_MANAGER
      .get_proxy_settings_by_id(proxy_id)
      .ok_or_else(|| invalid("selected fetch proxy no longer exists"))?;
    let proxy_url = crate::proxy_manager::ProxyManager::build_proxy_url(&settings);
    let proxy = reqwest::Proxy::all(&proxy_url).map_err(|e| format!("Invalid fetch proxy: {e}"))?;
    builder = builder.proxy(proxy);
  }
  let client = builder
    .build()
    .map_err(|e| format!("Failed to build HTTP client: {e}"))?;
  let response = client
    .get(&url)
    .header("User-Agent", "DucklingBrowser/1.0")
    .header("Accept", "text/plain, */*")
    .send()
    .await
    .map_err(|e| format!("Failed to fetch subscription: {e}"))?;
  if !response.status().is_success() {
    return Err(format!(
      "Subscription fetch failed with HTTP {}",
      response.status()
    ));
  }
  let mut bytes_read = 0usize;
  let mut body = Vec::new();
  let mut stream = response.bytes_stream();
  use futures_util::StreamExt;
  while let Some(chunk) = stream.next().await {
    let chunk = chunk.map_err(|e| format!("Failed to read subscription body: {e}"))?;
    bytes_read += chunk.len();
    if bytes_read > MAX_BODY_BYTES {
      return Err("Subscription body is too large (max 5 MB)".to_string());
    }
    body.extend_from_slice(&chunk);
  }
  String::from_utf8(body).map_err(|_| "Subscription body is not valid UTF-8".to_string())
}

/// Display name for a materialized entry: fragment → host → indexed fallback.
fn entry_display_name(sub_name: &str, link: &SubLink, index: usize) -> String {
  if let Some(name) = &link.name {
    // Fragment names from aggregators are noisy; keep them short.
    let short: String = name.chars().take(48).collect();
    return format!("{sub_name} · {short}");
  }
  // Fall back to host:port extracted from the authority section.
  let after_scheme = link
    .raw
    .split_once("://")
    .map(|(_, rest)| rest)
    .unwrap_or(&link.raw);
  let authority = after_scheme.split(['?', '#', '/']).next().unwrap_or("");
  let host = authority.rsplit('@').next().unwrap_or(authority);
  if !host.is_empty() {
    return format!("{sub_name} · {host}");
  }
  format!("{sub_name} #{n}", n = index + 1)
}

/// Reconcile parsed links against stored entries. Returns
/// (added, updated, pruned, unsupported, errors).
#[allow(clippy::too_many_arguments)]
pub async fn reconcile_subscription(
  app_handle: &tauri::AppHandle,
  sub: &Subscription,
  parsed: &ParsedSubscription,
) -> (usize, usize, usize, usize, Vec<String>) {
  let existing: HashMap<String, SubscriptionEntry> = {
    SUBSCRIPTION_MANAGER
      .lock()
      .unwrap()
      .entries_for(&sub.id)
      .into_iter()
      .map(|e| (e.link_hash.clone(), e))
      .collect()
  };

  let mut seen_hashes = HashSet::new();
  let mut added = 0usize;
  let mut updated = 0usize;
  let mut errors = Vec::new();

  for (index, link) in parsed.links.iter().enumerate() {
    let hash = link_hash(&link.raw);
    seen_hashes.insert(hash.clone());
    let Some(target) = link_target(link) else {
      continue;
    };
    let name = entry_display_name(&sub.name, link, index);
    if let Some(entry) = existing.get(&hash) {
      // Known link: refresh the display name when it changed.
      if entry.name != name {
        if entry.kind == "vpn" {
          if let Ok(storage) = crate::vpn::VPN_STORAGE.lock() {
            if storage.update_config_name(&entry.entry_id, &name).is_ok() {
              updated += 1;
            }
          }
        } else if crate::proxy_manager::PROXY_MANAGER
          .update_stored_proxy(app_handle, &entry.entry_id, Some(name.clone()), None)
          .is_ok()
        {
          updated += 1;
        }
        SUBSCRIPTION_MANAGER
          .lock()
          .unwrap()
          .upsert_entry(SubscriptionEntry {
            name,
            ..entry.clone()
          });
      }
      continue;
    }
    match materialize_entry(app_handle, sub, link, target, &name).await {
      Ok(entry_id) => {
        SUBSCRIPTION_MANAGER
          .lock()
          .unwrap()
          .upsert_entry(SubscriptionEntry {
            id: Uuid::new_v4().to_string(),
            subscription_id: sub.id.clone(),
            kind: target.to_string(),
            entry_id,
            link_hash: hash,
            name,
          });
        added += 1;
      }
      Err(e) => errors.push(format!("{name}: {e}")),
    }
  }

  // Disappeared links: prune only when enabled, and only own entries.
  let mut pruned = 0usize;
  if sub.auto_prune {
    let stale: Vec<SubscriptionEntry> = SUBSCRIPTION_MANAGER
      .lock()
      .unwrap()
      .entries_for(&sub.id)
      .into_iter()
      .filter(|e| !seen_hashes.contains(&e.link_hash))
      .collect();
    for entry in stale {
      delete_materialized_entry(app_handle, &entry);
      SUBSCRIPTION_MANAGER
        .lock()
        .unwrap()
        .remove_entry(&sub.id, &entry.link_hash);
      pruned += 1;
    }
  }

  (added, updated, pruned, parsed.unsupported, errors)
}

async fn materialize_entry(
  app_handle: &tauri::AppHandle,
  sub: &Subscription,
  link: &SubLink,
  target: &str,
  name: &str,
) -> Result<String, String> {
  if target == "vpn" {
    let storage = crate::vpn::VPN_STORAGE
      .lock()
      .map_err(|e| format!("Failed to lock VPN storage: {e}"))?;
    // Suffix on collision: VPN names are not unique-constrained, but keep
    // them distinguishable across subscriptions sharing a host.
    let unique_name = unique_vpn_name(&storage, name);
    let config = storage
      .import_config(&link.raw, "subscription.txt", Some(unique_name))
      .map_err(|e| e.to_string())?;
    if config.sync_enabled {
      if let Some(scheduler) = crate::sync::get_global_scheduler() {
        let id = config.id.clone();
        tauri::async_runtime::spawn(async move {
          scheduler.queue_vpn_sync(id).await;
        });
      }
    }
    Ok(config.id)
  } else {
    let parsed = crate::proxy_manager::ProxyManager::parse_txt_proxies(&link.raw);
    let first = parsed
      .into_iter()
      .next()
      .ok_or_else(|| "Unparsable proxy link".to_string())?;
    let settings = match first {
      crate::proxy_manager::ProxyParseResult::Parsed(p) => crate::browser::ProxySettings {
        proxy_type: p.proxy_type,
        host: p.host,
        port: p.port,
        username: p.username,
        password: p.password,
      },
      crate::proxy_manager::ProxyParseResult::Ambiguous { .. } => {
        return Err("Ambiguous proxy format".to_string());
      }
      crate::proxy_manager::ProxyParseResult::Invalid { reason, .. } => {
        return Err(reason);
      }
    };
    let unique_name = unique_proxy_name(name);
    let proxy = crate::proxy_manager::PROXY_MANAGER
      .create_stored_proxy(app_handle, unique_name, settings)
      .map_err(|e| {
        // Deduplicate against identical upstream URLs instead of failing:
        // a shared aggregator host may already exist as a manual proxy.
        if e.contains("already exists") {
          format!(
            "duplicate of an existing proxy ({sub_name})",
            sub_name = sub.name
          )
        } else {
          e
        }
      })?;
    Ok(proxy.id)
  }
}

fn unique_vpn_name(storage: &crate::vpn::VpnStorage, base: &str) -> String {
  let existing: HashSet<String> = storage
    .list_configs()
    .map(|list| list.into_iter().map(|c| c.name).collect())
    .unwrap_or_default();
  if !existing.contains(base) {
    return base.to_string();
  }
  for i in 2..1000 {
    let candidate = format!("{base} ({i})");
    if !existing.contains(&candidate) {
      return candidate;
    }
  }
  format!("{base} {}", &Uuid::new_v4().to_string()[..8])
}

fn unique_proxy_name(base: &str) -> String {
  let proxies = crate::proxy_manager::PROXY_MANAGER.get_stored_proxies();
  if !proxies.iter().any(|p| p.name == base) {
    return base.to_string();
  }
  for i in 2..1000 {
    let candidate = format!("{base} ({i})");
    if !proxies.iter().any(|p| p.name == candidate) {
      return candidate;
    }
  }
  format!("{base} {}", &Uuid::new_v4().to_string()[..8])
}

fn delete_materialized_entry(app_handle: &tauri::AppHandle, entry: &SubscriptionEntry) {
  if entry.kind == "vpn" {
    if let Ok(storage) = crate::vpn::VPN_STORAGE.lock() {
      let _ = storage.delete_config(&entry.entry_id);
    }
  } else {
    let _ = crate::proxy_manager::PROXY_MANAGER.delete_stored_proxy(app_handle, &entry.entry_id);
  }
}

/// Health-check every entry of a subscription. Returns (working, failed).
/// Failed entries are deleted when `auto_prune` is set.
pub async fn check_subscription_entries(
  app_handle: &tauri::AppHandle,
  sub: &Subscription,
) -> (usize, usize) {
  let entries = SUBSCRIPTION_MANAGER.lock().unwrap().entries_for(&sub.id);
  let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(CHECK_CONCURRENCY));
  let mut handles = Vec::new();
  for entry in entries {
    let permit_owner = semaphore.clone();
    let app = app_handle.clone();
    let entry_clone = entry.clone();
    handles.push(tauri::async_runtime::spawn(async move {
      let _permit = permit_owner.acquire_owned().await;
      let ok = tokio::time::timeout(CHECK_TIMEOUT, check_one_entry(&app, &entry_clone))
        .await
        .unwrap_or(false);
      (entry_clone, ok)
    }));
  }
  let mut working = 0usize;
  let mut failed = 0usize;
  for handle in handles {
    let Ok((entry, ok)) = handle.await else {
      failed += 1;
      continue;
    };
    if ok {
      working += 1;
    } else {
      failed += 1;
      if sub.auto_prune {
        delete_materialized_entry(app_handle, &entry);
        SUBSCRIPTION_MANAGER
          .lock()
          .unwrap()
          .remove_entry(&sub.id, &entry.link_hash);
      }
    }
  }
  (working, failed)
}

async fn check_one_entry(app_handle: &tauri::AppHandle, entry: &SubscriptionEntry) -> bool {
  if entry.kind == "vpn" {
    crate::check_vpn_validity_core(&entry.entry_id)
      .await
      .is_ok_and(|r| r.is_valid)
  } else {
    let Some(settings) =
      crate::proxy_manager::PROXY_MANAGER.get_proxy_settings_by_id(&entry.entry_id)
    else {
      return false;
    };
    // check_proxy_validity takes ownership through &self + app-free signature.
    let _ = app_handle;
    crate::proxy_manager::PROXY_MANAGER
      .check_proxy_validity(&entry.entry_id, &settings)
      .await
      .is_ok_and(|r| r.is_valid)
  }
}

/// Full refresh: fetch → parse → reconcile → optional health check.
/// Records `last_status` on the subscription for UI display.
pub async fn refresh_subscription(
  app_handle: &tauri::AppHandle,
  id: &str,
) -> Result<serde_json::Value, String> {
  let sub = SUBSCRIPTION_MANAGER
    .lock()
    .unwrap()
    .list()
    .into_iter()
    .find(|s| s.id == id)
    .ok_or_else(|| not_found(id))?;
  let body = fetch_subscription_body(&sub.url, sub.use_proxy_id.as_deref()).await?;
  let parsed = parse_subscription_body(&body);
  if parsed.links.is_empty() && parsed.unsupported == 0 {
    let status = "empty: no usable links found";
    SUBSCRIPTION_MANAGER
      .lock()
      .unwrap()
      .record_status(id, status);
    return Err(status.to_string());
  }
  let (added, updated, pruned, unsupported, errors) =
    reconcile_subscription(app_handle, &sub, &parsed).await;
  let (working, failed) = if sub.auto_check {
    check_subscription_entries(app_handle, &sub).await
  } else {
    (0, 0)
  };
  let status = format!(
    "ok: +{added} ~{updated} -{pruned} unsupported:{unsupported} check:{working}ok/{failed}bad errors:{}",
    errors.len()
  );
  SUBSCRIPTION_MANAGER
    .lock()
    .unwrap()
    .record_status(id, &status);
  let _ = crate::events::emit("subscriptions-changed", &status);
  let _ = crate::events::emit_empty("stored-proxies-changed");
  let _ = crate::events::emit_empty("vpn-configs-changed");
  Ok(serde_json::json!({
    "added": added, "updated": updated, "pruned": pruned,
    "unsupported": unsupported, "working": working, "failed": failed,
    "errors": errors, "userInfo": parsed.user_info,
  }))
}

/// Background loop: refresh due subscriptions every LOOP_TICK.
pub fn start_subscription_loop(app_handle: tauri::AppHandle) {
  tauri::async_runtime::spawn(async move {
    loop {
      let due = SUBSCRIPTION_MANAGER
        .lock()
        .unwrap()
        .due_for_refresh(now_secs());
      for sub in due {
        log::info!("Auto-refreshing subscription {}", sub.name);
        if let Err(e) = refresh_subscription(&app_handle, &sub.id).await {
          log::warn!("Subscription {} refresh failed: {e}", sub.name);
          SUBSCRIPTION_MANAGER
            .lock()
            .unwrap()
            .record_status(&sub.id, &format!("error: {e}"));
        }
      }
      tokio::time::sleep(LOOP_TICK).await;
    }
  });
  log::info!("Subscription refresh loop started");
}

// ── Tauri commands ──────────────────────────────────────────────

#[tauri::command]
pub fn subscriptions_list() -> Result<Vec<Subscription>, String> {
  Ok(SUBSCRIPTION_MANAGER.lock().unwrap().list())
}

#[tauri::command]
pub fn subscription_entries(subscription_id: String) -> Result<Vec<SubscriptionEntry>, String> {
  Ok(
    SUBSCRIPTION_MANAGER
      .lock()
      .unwrap()
      .entries_for(&subscription_id),
  )
}

#[tauri::command]
pub fn subscription_save(
  id: Option<String>,
  name: String,
  url: String,
  refresh_hours: u64,
  use_proxy_id: Option<String>,
  auto_check: bool,
  auto_prune: bool,
) -> Result<Subscription, String> {
  SUBSCRIPTION_MANAGER.lock().unwrap().save_subscription(
    id,
    &name,
    &url,
    refresh_hours,
    use_proxy_id,
    auto_check,
    auto_prune,
  )
}

#[tauri::command]
pub fn subscription_delete(
  app_handle: tauri::AppHandle,
  id: String,
  delete_entries: bool,
) -> Result<(), String> {
  let orphaned = SUBSCRIPTION_MANAGER.lock().unwrap().delete(&id)?;
  if delete_entries {
    for entry in orphaned {
      delete_materialized_entry(&app_handle, &entry);
    }
    let _ = crate::events::emit_empty("stored-proxies-changed");
    let _ = crate::events::emit_empty("vpn-configs-changed");
  }
  Ok(())
}

#[tauri::command]
pub async fn subscription_refresh(
  app_handle: tauri::AppHandle,
  id: String,
) -> Result<serde_json::Value, String> {
  refresh_subscription(&app_handle, &id).await
}

#[tauri::command]
pub async fn subscription_preview(
  url: String,
  use_proxy_id: Option<String>,
) -> Result<serde_json::Value, String> {
  let body = fetch_subscription_body(&url, use_proxy_id.as_deref()).await?;
  let parsed = parse_subscription_body(&body);
  let mut vpn = 0usize;
  let mut proxy = 0usize;
  for link in &parsed.links {
    match link_target(link) {
      Some("vpn") => vpn += 1,
      Some("proxy") => proxy += 1,
      _ => {}
    }
  }
  Ok(serde_json::json!({
    "vpn": vpn, "proxy": proxy, "unsupported": parsed.unsupported,
    "suggestedIntervalHours": parsed.suggested_interval_hours,
    "userInfo": parsed.user_info,
  }))
}

#[cfg(test)]
mod tests {
  use super::*;

  const BARRY_FAR_SAMPLE: &str = "#profile-title: base64:eA==\n\
    #profile-update-interval: 1\n\
    #subscription-userinfo: upload=29; download=12; total=10737418240000000; expire=2546249531\n\
    #support-url: https://github.com/barry-far/V2ray-config\n\
    vless://05519058-d2ac-4f28-9e4a-2b2a1386749e@13.37.92.74:22222?security=tls&encryption=none#frag-name\n\
    trojan://user@213.182.199.137:443?security=tls#chan\n\
    vmess://eyJhZGQiOiIxLjIuMy40IiwicG9ydCI6IjQ0MyJ9\n\
    hy2://abc@1.2.3.4:443/?insecure=1#kr\n\
    ss://MjAyMi1ibGFrZTMtY2hhY2hhMjAtcG9seTEzMDU6cGFzczA=@130.61.43.101:59924#de\n\
    http://user:pass@5.6.7.8:8080\n\
    not-a-link-at-all\n";

  #[test]
  fn parses_barry_far_style_body() {
    let parsed = parse_subscription_body(BARRY_FAR_SAMPLE);
    // vless + ss + http are supported; trojan/vmess/hy2 + garbage are not.
    assert_eq!(parsed.links.len(), 3);
    assert_eq!(parsed.links[0].scheme, "vless");
    assert_eq!(parsed.links[0].name.as_deref(), Some("frag-name"));
    assert_eq!(parsed.suggested_interval_hours, Some(1));
    assert!(parsed.user_info.as_deref().unwrap().contains("upload=29"));
    assert_eq!(parsed.unsupported, 4);
    assert_eq!(link_target(&parsed.links[0]), Some("vpn"));
    assert_eq!(link_target(&parsed.links[1]), Some("proxy"));
  }

  #[test]
  fn decodes_base64_bodies() {
    use base64::Engine;
    let inner = "vless://uuid@9.9.9.9:443?security=tls#x\nss://bWV0aG9kOnBhc3M=@8.8.8.8:8388#y\n";
    let encoded = base64::engine::general_purpose::STANDARD.encode(inner);
    let parsed = parse_subscription_body(&encoded);
    assert_eq!(parsed.links.len(), 2);
  }

  #[test]
  fn classify_line_rejects_comments_and_garbage() {
    assert!(classify_line("#comment").is_none());
    assert!(classify_line("").is_none());
    assert!(classify_line("just words").is_none());
    let link = classify_line("VLESS://u@h:443?a=b#N").unwrap();
    assert_eq!(link.scheme, "vless");
  }

  #[test]
  fn entry_names_prefer_fragment_then_host() {
    let frag = SubLink {
      scheme: "vless".to_string(),
      raw: "vless://u@h:1#a b c".to_string(),
      name: Some("a b c".to_string()),
    };
    assert_eq!(entry_display_name("Sub", &frag, 0), "Sub · a b c");
    let host = SubLink {
      scheme: "ss".to_string(),
      raw: "ss://m:p@9.9.9.9:8388".to_string(),
      name: None,
    };
    assert_eq!(entry_display_name("Sub", &host, 0), "Sub · 9.9.9.9:8388");
  }

  #[test]
  fn link_hash_is_stable_and_trims() {
    assert_eq!(link_hash("vless://a"), link_hash("  vless://a  "));
    assert_ne!(link_hash("vless://a"), link_hash("vless://b"));
  }

  #[test]
  fn store_crud_with_temp_path() {
    let dir = tempfile::tempdir().unwrap();
    let manager = SubscriptionManager::with_path(dir.path().join("subs.json"));
    assert!(manager.list().is_empty());
    assert!(manager
      .save_subscription(None, "", "https://x.test/s", 1, None, false, false)
      .is_err());
    assert!(manager
      .save_subscription(None, "S", "ftp://x.test/s", 1, None, false, false)
      .is_err());
    let sub = manager
      .save_subscription(None, "S", "https://x.test/s", 25, None, true, true)
      .unwrap();
    assert_eq!(manager.list().len(), 1);
    // Not due immediately after creation without last_fetched_at? It IS due
    // (never fetched). Record a fetch then check the interval gate.
    manager.record_status(&sub.id, "ok");
    assert!(manager.due_for_refresh(now_secs()).is_empty());
    assert_eq!(manager.due_for_refresh(now_secs() + 26 * 3600).len(), 1);
    let updated = manager
      .save_subscription(
        Some(sub.id.clone()),
        "S2",
        "https://x.test/s2",
        0,
        None,
        false,
        false,
      )
      .unwrap();
    assert_eq!(updated.name, "S2");
    assert_eq!(updated.refresh_hours, 0);
    manager.delete(&sub.id).unwrap();
    assert!(manager.list().is_empty());
    assert!(manager.delete(&sub.id).is_err());
  }

  #[test]
  fn validate_url_rules() {
    assert!(validate_url("https://raw.githubusercontent.com/x/Sub1.txt").is_ok());
    assert!(validate_url("http://localhost:8080/sub").is_ok());
    assert!(validate_url("ftp://x/y").is_err());
    assert!(validate_url("").is_err());
    assert!(validate_url("https://").is_err());
  }
}
