use crate::browser::{create_browser, BrowserType};
use crate::chromium_manager::ChromiumConfig;
use crate::cloud_auth::CLOUD_AUTH;
use crate::downloaded_browsers_registry::DownloadedBrowsersRegistry;
use crate::events;
use crate::profile::types::{get_host_os, BrowserProfile, SyncMode};
use crate::proxy_manager::PROXY_MANAGER;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{self, create_dir_all};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::sync::Mutex;
use std::time::SystemTime;
use sysinfo::{Pid, ProcessRefreshKind, RefreshKind, System};
use url::Url;

fn atomic_write(path: &Path, data: &[u8]) -> std::io::Result<()> {
  let tmp = path.with_extension(match path.extension().and_then(|e| e.to_str()) {
    Some(ext) => format!("{ext}.tmp"),
    None => "tmp".to_string(),
  });
  {
    let mut f = fs::File::create(&tmp)?;
    use std::io::Write;
    f.write_all(data)?;
    f.sync_all()?;
  }
  fs::rename(&tmp, path)
}

/// Parsed profile plus the mtime of its `metadata.json` at cache time.
struct CachedProfileEntry {
  profile: BrowserProfile,
  profile_dir: PathBuf,
  mtime: Option<SystemTime>,
}

/// Per-store-directory index. Keyed by base dir so concurrent tests or a
/// settings change never let one base dir's churn invalidate another's cache.
#[derive(Default)]
struct BaseCache {
  dir_mtime: Option<SystemTime>,
  entries: HashMap<uuid::Uuid, CachedProfileEntry>,
}

/// Cached profile-store index (name→id is derivable from the parsed profile;
/// this holds id→parsed-profile + path so `list_profiles` skips re-reading
/// `metadata.json` while the store is unchanged). O(n) parse cost on first
/// load (and after any directory change) only.
#[derive(Default)]
struct ProfileIndexCache {
  bases: HashMap<PathBuf, BaseCache>,
}

static PROFILE_INDEX_CACHE: LazyLock<Mutex<ProfileIndexCache>> =
  LazyLock::new(|| Mutex::new(ProfileIndexCache::default()));

fn entry_is_fresh(entry: &CachedProfileEntry) -> bool {
  match fs::metadata(entry.profile_dir.join("metadata.json")).and_then(|m| m.modified()) {
    Ok(mtime) => Some(mtime) == entry.mtime,
    Err(_) => false,
  }
}

impl ProfileIndexCache {
  fn base(&mut self, profiles_dir: &Path) -> &mut BaseCache {
    self.bases.entry(profiles_dir.to_path_buf()).or_default()
  }

  /// Returns a snapshot of every profile for the given base when the index is
  /// fresh: same base dir, same directory mtime, and every cached
  /// `metadata.json` unchanged. Self-heals externally edited files by
  /// re-reading just those entries. A suspicious entry (vanished file, failed
  /// parse) makes the caller rescan the whole store rather than silently
  /// dropping data.
  fn snapshot_if_fresh(&mut self, profiles_dir: &Path) -> Option<Vec<BrowserProfile>> {
    let base = self.base(profiles_dir);
    let current_dir_mtime = fs::metadata(profiles_dir)
      .ok()
      .and_then(|m| m.modified().ok());
    if base.dir_mtime != current_dir_mtime {
      return None;
    }

    let mut profiles = Vec::with_capacity(base.entries.len());
    for entry in base.entries.values_mut() {
      let metadata_file = entry.profile_dir.join("metadata.json");
      match fs::metadata(&metadata_file).and_then(|m| m.modified()) {
        Ok(mtime) if Some(mtime) == entry.mtime => {
          profiles.push(entry.profile.clone());
        }
        Ok(_) => {
          // External write: re-read this single file and refresh the entry.
          match parse_metadata_file(&metadata_file) {
            Some(profile) => {
              entry.profile = profile.clone();
              entry.mtime = fs::metadata(&metadata_file)
                .ok()
                .and_then(|m| m.modified().ok());
              profiles.push(profile);
            }
            None => {
              log::warn!(
                "Profile index: skipping invalid metadata at {}, rescanning base",
                metadata_file.display()
              );
              return None;
            }
          }
        }
        Err(_) => return None,
      }
    }

    Some(profiles)
  }
}

/// Parse one `metadata.json`, applying the host_os backfill so cached entries
/// are equivalent to a fresh scan.
fn parse_metadata_file(metadata_file: &Path) -> Option<BrowserProfile> {
  let content = match fs::read_to_string(metadata_file) {
    Ok(c) => c,
    Err(e) => {
      log::warn!(
        "Skipping profile at {}: failed to read metadata.json: {e}",
        metadata_file.display()
      );
      return None;
    }
  };
  let mut profile: BrowserProfile = match serde_json::from_str(&content) {
    Ok(p) => p,
    Err(e) => {
      log::warn!(
        "Skipping profile at {}: invalid metadata.json: {e}",
        metadata_file.display()
      );
      return None;
    }
  };

  // Backfill host_os from browser config for profiles created before
  // the field existed (or synced without it).
  if profile.host_os.is_none() {
    let inferred_os = profile.resolved_os().map(str::to_string);
    if let Some(os) = inferred_os {
      profile.host_os = Some(os);
      if let Ok(json) = serde_json::to_string_pretty(&profile) {
        let _ = atomic_write(metadata_file, json.as_bytes());
      }
    }
  }

  Some(profile)
}

fn default_release_type() -> String {
  "stable".to_string()
}

/// One profile entry in a batch-create request/response. Per-item so a single
/// failure (e.g. a duplicate name) never aborts the rest of the batch.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchCreateResult {
  pub name: String,
  pub ok: bool,
  pub profile: Option<BrowserProfile>,
  pub error: Option<String>,
}

/// Shared template for creating many profiles in one call. `names` wins when
/// non-empty; otherwise `name_prefix` + `count` generates `"{prefix} {i}"`
/// names. A provided `chromium_config.fingerprint` skips fingerprint
/// generation entirely; otherwise one fingerprint is generated for the batch
/// and shared by every profile.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct BatchCreateProfilesRequest {
  pub names: Vec<String>,
  pub name_prefix: Option<String>,
  pub count: Option<u32>,
  pub browser: String,
  pub version: String,
  pub release_type: String,
  pub proxy_id: Option<String>,
  pub vpn_id: Option<String>,
  pub chromium_config: Option<ChromiumConfig>,
  pub group_id: Option<String>,
  pub ephemeral: bool,
  pub dns_blocklist: Option<String>,
  pub launch_hook: Option<String>,
}

impl Default for BatchCreateProfilesRequest {
  fn default() -> Self {
    Self {
      names: Vec::new(),
      name_prefix: None,
      count: None,
      browser: String::new(),
      version: String::new(),
      release_type: default_release_type(),
      proxy_id: None,
      vpn_id: None,
      chromium_config: None,
      group_id: None,
      ephemeral: false,
      dns_blocklist: None,
      launch_hook: None,
    }
  }
}

pub struct ProfileManager {
  chromium_manager: &'static crate::chromium_manager::ChromiumManager,
}

impl ProfileManager {
  fn new() -> Self {
    Self {
      chromium_manager: crate::chromium_manager::ChromiumManager::instance(),
    }
  }

  pub fn instance() -> &'static ProfileManager {
    &PROFILE_MANAGER
  }

  pub fn get_profiles_dir(&self) -> PathBuf {
    crate::app_dirs::profiles_dir()
  }

  pub fn get_binaries_dir(&self) -> PathBuf {
    crate::app_dirs::binaries_dir()
  }

  fn normalize_launch_hook(
    launch_hook: Option<String>,
  ) -> Result<Option<String>, Box<dyn std::error::Error>> {
    let Some(raw) = launch_hook else {
      return Ok(None);
    };

    let trimmed = raw.trim();
    if trimmed.is_empty() {
      return Ok(None);
    }

    let parsed = Url::parse(trimmed).map_err(|e| format!("Invalid launch hook URL: {e}"))?;
    match parsed.scheme() {
      "http" | "https" => Ok(Some(parsed.to_string())),
      _ => Err("Launch hook URL must use http or https".into()),
    }
  }

  #[allow(clippy::too_many_arguments)]
  pub async fn create_profile_with_group(
    &self,
    app_handle: &tauri::AppHandle,
    name: &str,
    browser: &str,
    version: &str,
    release_type: &str,
    proxy_id: Option<String>,
    vpn_id: Option<String>,
    chromium_config: Option<ChromiumConfig>,
    group_id: Option<String>,
    ephemeral: bool,
    dns_blocklist: Option<String>,
    launch_hook: Option<String>,
  ) -> Result<BrowserProfile, Box<dyn std::error::Error>> {
    if name.trim().is_empty() {
      return Err(
        serde_json::json!({ "code": "NAME_CANNOT_BE_EMPTY" })
          .to_string()
          .into(),
      );
    }

    if proxy_id.is_some() && vpn_id.is_some() {
      return Err("Cannot set both proxy_id and vpn_id".into());
    }

    let launch_hook = Self::normalize_launch_hook(launch_hook)?;

    // Sync cloud proxy credentials if the profile uses a cloud or cloud-derived proxy
    if let Some(ref pid) = proxy_id {
      if PROXY_MANAGER.is_cloud_or_derived(pid) || pid == crate::proxy_manager::CLOUD_PROXY_ID {
        log::info!("Syncing cloud proxy credentials before profile creation");
        CLOUD_AUTH.sync_cloud_proxy().await;
      }
    }

    log::info!("Attempting to create profile: {name}");

    if browser == "camoufox" {
      return Err(
        serde_json::json!({ "code": "CAMOUFOX_REMOVED" })
          .to_string()
          .into(),
      );
    }

    // Check if a profile with this name already exists (case insensitive)
    let existing_profiles = self.list_profiles()?;
    if existing_profiles
      .iter()
      .any(|p| p.name.to_lowercase() == name.to_lowercase())
    {
      return Err(format!("Profile with name '{name}' already exists").into());
    }

    // Generate a new UUID for this profile
    let profile_id = uuid::Uuid::new_v4();
    let profiles_dir = self.get_profiles_dir();
    let profile_uuid_dir = profiles_dir.join(profile_id.to_string());
    let profile_data_dir = profile_uuid_dir.join("profile");
    let profile_file = profile_uuid_dir.join("metadata.json");

    // Create profile directory with UUID and profile subdirectory
    create_dir_all(&profile_uuid_dir)?;
    if !ephemeral {
      create_dir_all(&profile_data_dir)?;
    }

    // For Chromium profiles, generate fingerprint during creation
    let final_chromium_config = if browser == "chromium" {
      let mut config = chromium_config.unwrap_or_else(|| {
        log::info!("Creating default browser config for profile: {name}");
        crate::chromium_manager::ChromiumConfig::default()
      });

      // Always ensure executable_path is set to the user's binary location
      // Pass upstream proxy information to config for fingerprint generation
      if let Some(proxy_id_ref) = &proxy_id {
        if let Some(proxy_settings) = PROXY_MANAGER.get_proxy_settings_by_id(proxy_id_ref) {
          let proxy_url = if let (Some(username), Some(password)) =
            (&proxy_settings.username, &proxy_settings.password)
          {
            format!(
              "{}://{}:{}@{}:{}",
              proxy_settings.proxy_type.to_lowercase(),
              username,
              password,
              proxy_settings.host,
              proxy_settings.port
            )
          } else {
            format!(
              "{}://{}:{}",
              proxy_settings.proxy_type.to_lowercase(),
              proxy_settings.host,
              proxy_settings.port
            )
          };
          config.proxy = Some(proxy_url);
          log::info!(
            "Using upstream proxy for fingerprint generation: {}://{}:{}",
            proxy_settings.proxy_type.to_lowercase(),
            proxy_settings.host,
            proxy_settings.port
          );
        }
      }

      // Whether the fingerprint's location fields are known to match the
      // profile's routing. Provided fingerprints keep the old stamping
      // behavior; for generated ones this comes from the geolocation lookup.
      let mut geolocation_applied = true;

      // Generate fingerprint if not already provided
      if config.fingerprint.is_none() {
        log::info!("Generating fingerprint for profile: {name}");

        // Create a temporary profile for fingerprint generation
        let temp_profile = BrowserProfile {
          id: uuid::Uuid::new_v4(),
          name: name.to_string(),
          browser: browser.to_string(),
          version: version.to_string(),
          proxy_id: proxy_id.clone(),
          vpn_id: None,
          launch_hook: launch_hook.clone(),
          process_id: None,
          last_launch: None,
          release_type: release_type.to_string(),
          chromium_config: None,
          group_id: group_id.clone(),
          tags: Vec::new(),
          note: None,
          window_color: None,
          sync_mode: SyncMode::Disabled,
          encryption_salt: None,
          last_sync: None,
          host_os: None,
          ephemeral: false,
          extension_group_id: None,
          proxy_bypass_rules: Vec::new(),
          created_by_id: None,
          created_by_email: None,
          dns_blocklist: None,
          password_protected: false,
          clear_on_close: false,
          created_at: None,
          updated_at: None,
          download_dir: None,
          allow_agent_downloads: false,
          agent_auto_approve: false,
          agent_key_id: None,
          agent_id: None,
        };

        match self
          .chromium_manager
          .generate_fingerprint_config(app_handle, &temp_profile, &config)
          .await
        {
          Ok((generated_fingerprint, geo_applied)) => {
            config.fingerprint = Some(generated_fingerprint);
            geolocation_applied = geo_applied;
            log::info!("Successfully generated fingerprint for profile: {name}");
          }
          Err(e) => {
            return Err(format!("Failed to generate fingerprint for profile '{name}': {e}").into());
          }
        }
      } else {
        log::info!("Using provided fingerprint for profile: {name}");
      }

      // Record which proxy/geoip the fingerprint's location data was computed
      // for. On launch this is compared against the profile's current routing
      // so a proxy that was changed after creation triggers a location refresh
      // instead of showing a stale timezone. Only stamped when geolocation
      // actually succeeded: on failure the fingerprint carries the HOST
      // timezone/locale, and a stamped signature would match at launch and
      // suppress the refresh that repairs it — latching the leak permanently.
      config.geo_proxy_signature = if geolocation_applied {
        Some(crate::chromium_manager::ChromiumManager::geo_signature(
          proxy_id
            .as_ref()
            .and_then(|id| PROXY_MANAGER.get_proxy_settings_by_id(id))
            .as_ref(),
          None,
          config.geoip.as_ref(),
        ))
      } else {
        if !matches!(config.geoip.as_ref(), Some(serde_json::Value::Bool(false))) {
          log::warn!(
            "Geolocation could not be applied for profile {name}; leaving geo signature unset so the next launch refreshes location through the profile's proxy"
          );
        }
        None
      };

      // Clear the proxy from config after fingerprint generation
      config.proxy = None;

      Some(config)
    } else {
      chromium_config.clone()
    };

    let profile = BrowserProfile {
      id: profile_id,
      name: name.to_string(),
      browser: browser.to_string(),
      version: version.to_string(),
      proxy_id: proxy_id.clone(),
      vpn_id: vpn_id.clone(),
      launch_hook,
      process_id: None,
      last_launch: None,
      release_type: release_type.to_string(),
      chromium_config: final_chromium_config,
      group_id: group_id.clone(),
      tags: Vec::new(),
      note: None,
      // A random-looking pastel derived from the (random) profile id, so every
      // new profile gets a distinct, stable window color it can later override.
      window_color: Some(crate::chromium_manager::derive_profile_color(&profile_id)),
      sync_mode: SyncMode::Disabled,
      encryption_salt: None,
      last_sync: None,
      host_os: Some(get_host_os()),
      ephemeral,
      extension_group_id: None,
      proxy_bypass_rules: Vec::new(),
      created_by_id: None,
      created_by_email: None,
      dns_blocklist,
      password_protected: false,
      clear_on_close: false,
      created_at: Some(
        std::time::SystemTime::now()
          .duration_since(std::time::UNIX_EPOCH)
          .map(|d| d.as_secs())
          .unwrap_or(0),
      ),
      updated_at: Some(crate::proxy_manager::now_secs()),
      download_dir: None,
      allow_agent_downloads: false,
      agent_auto_approve: false,
      agent_key_id: None,
      agent_id: None,
    };

    // Save profile info
    self.save_profile(&profile)?;

    // Verify the profile was saved correctly
    if !profile_file.exists() {
      return Err(format!("Failed to create profile file for '{name}'").into());
    }

    log::info!("Profile '{name}' created successfully with ID: {profile_id}");

    // Emit profile creation event
    if let Err(e) = events::emit_empty("profiles-changed") {
      log::warn!("Warning: Failed to emit profiles-changed event: {e}");
    }

    Ok(profile)
  }

  /// Hard cap on profiles created per batch request. Keeps one request from
  /// overwhelming the host; callers loop for larger fleets.
  pub const MAX_BATCH_CREATE_PROFILES: usize = 500;

  /// Resolve the final profile-name list for a batch request. Explicit
  /// `names` win when non-empty; otherwise `name_prefix` + `count` generates
  /// `"{prefix} {i}"` names. Whitespace-only names are dropped.
  pub(crate) fn resolve_batch_names(
    names: &[String],
    name_prefix: Option<&str>,
    count: Option<u32>,
  ) -> Result<Vec<String>, String> {
    let mut resolved: Vec<String> = Vec::new();
    if !names.is_empty() {
      resolved.extend(
        names
          .iter()
          .map(|n| n.trim().to_string())
          .filter(|n| !n.is_empty()),
      );
    } else if let (Some(prefix), Some(count)) = (name_prefix, count) {
      let prefix = prefix.trim().to_string();
      if prefix.is_empty() {
        return Err(serde_json::json!({ "code": "BATCH_CREATE_EMPTY_PREFIX" }).to_string());
      }
      resolved = (1..=count).map(|i| format!("{prefix} {i}")).collect();
    }
    if resolved.is_empty() {
      return Err(serde_json::json!({ "code": "BATCH_CREATE_NO_NAMES" }).to_string());
    }
    if resolved.len() > Self::MAX_BATCH_CREATE_PROFILES {
      return Err(
        serde_json::json!({
          "code": "BATCH_CREATE_TOO_MANY",
          "params": {
            "max": Self::MAX_BATCH_CREATE_PROFILES,
            "requested": resolved.len()
          }
        })
        .to_string(),
      );
    }
    Ok(resolved)
  }

  /// Create many profiles from one shared template. The expensive steps —
  /// fingerprint generation and the geolocation lookup — happen exactly once
  /// per batch; per-profile work is directory + metadata writes only. Results
  /// are per-name so one failure never aborts the rest of the batch.
  pub async fn batch_create_profiles(
    &self,
    app_handle: &tauri::AppHandle,
    req: &BatchCreateProfilesRequest,
  ) -> Result<Vec<BatchCreateResult>, Box<dyn std::error::Error + Send + Sync>> {
    let names = Self::resolve_batch_names(&req.names, req.name_prefix.as_deref(), req.count)?;

    if req.browser == "camoufox" {
      return Err(
        serde_json::json!({ "code": "CAMOUFOX_REMOVED" })
          .to_string()
          .into(),
      );
    }
    if req.proxy_id.is_some() && req.vpn_id.is_some() {
      return Err("Cannot set both proxy_id and vpn_id".into());
    }

    let launch_hook = Self::normalize_launch_hook(req.launch_hook.clone())
      .map_err(|e| Box::<dyn std::error::Error + Send + Sync>::from(e.to_string()))?;

    // Resolve the shared Chromium config once for the whole batch. A provided
    // fingerprint skips generation entirely; otherwise one fingerprint (with a
    // single geolocation lookup through the profile's proxy) is reused for
    // every profile in the batch.
    let base_config = if req.browser == "chromium" {
      let mut config = req.chromium_config.clone().unwrap_or_else(|| {
        log::info!("Creating default browser config for batch");
        crate::chromium_manager::ChromiumConfig::default()
      });

      if let Some(proxy_id_ref) = &req.proxy_id {
        if let Some(proxy_settings) = PROXY_MANAGER.get_proxy_settings_by_id(proxy_id_ref) {
          let proxy_url = if let (Some(username), Some(password)) =
            (&proxy_settings.username, &proxy_settings.password)
          {
            format!(
              "{}://{}:{}@{}:{}",
              proxy_settings.proxy_type.to_lowercase(),
              username,
              password,
              proxy_settings.host,
              proxy_settings.port
            )
          } else {
            format!(
              "{}://{}:{}",
              proxy_settings.proxy_type.to_lowercase(),
              proxy_settings.host,
              proxy_settings.port
            )
          };
          config.proxy = Some(proxy_url);
        }
      }

      let mut geolocation_applied = true;
      if config.fingerprint.is_none() {
        log::info!(
          "Generating one shared fingerprint for batch of {} profiles",
          names.len()
        );
        let temp_profile = BrowserProfile {
          id: uuid::Uuid::new_v4(),
          name: "batch-fingerprint-template".to_string(),
          browser: req.browser.clone(),
          version: req.version.clone(),
          proxy_id: req.proxy_id.clone(),
          vpn_id: None,
          launch_hook: launch_hook.clone(),
          process_id: None,
          last_launch: None,
          release_type: req.release_type.clone(),
          chromium_config: None,
          group_id: req.group_id.clone(),
          tags: Vec::new(),
          note: None,
          window_color: None,
          sync_mode: SyncMode::Disabled,
          encryption_salt: None,
          last_sync: None,
          host_os: None,
          ephemeral: false,
          extension_group_id: None,
          proxy_bypass_rules: Vec::new(),
          created_by_id: None,
          created_by_email: None,
          dns_blocklist: None,
          password_protected: false,
          clear_on_close: false,
          created_at: None,
          updated_at: None,
          download_dir: None,
          allow_agent_downloads: false,
          agent_auto_approve: false,
          agent_key_id: None,
          agent_id: None,
        };
        match self
          .chromium_manager
          .generate_fingerprint_config(app_handle, &temp_profile, &config)
          .await
        {
          Ok((generated_fingerprint, geo_applied)) => {
            config.fingerprint = Some(generated_fingerprint);
            geolocation_applied = geo_applied;
          }
          Err(e) => {
            return Err(format!("Failed to generate fingerprint for batch: {e}").into());
          }
        }
      } else {
        log::info!("Using provided fingerprint for batch");
      }

      config.geo_proxy_signature = if geolocation_applied {
        Some(crate::chromium_manager::ChromiumManager::geo_signature(
          req
            .proxy_id
            .as_ref()
            .and_then(|id| PROXY_MANAGER.get_proxy_settings_by_id(id))
            .as_ref(),
          None,
          config.geoip.as_ref(),
        ))
      } else {
        None
      };

      config.proxy = None;
      Some(config)
    } else {
      req.chromium_config.clone()
    };

    let existing_profiles = self
      .list_profiles()
      .map_err(|e| format!("Failed to list existing profiles: {e}"))?;
    let mut existing_names: std::collections::HashSet<String> = existing_profiles
      .iter()
      .map(|p| p.name.to_lowercase())
      .collect();

    let results = self.create_batch_many(names, req, base_config, launch_hook, &mut existing_names);

    // One tag-index rebuild and one event for the whole batch instead of one
    // per profile (the O(n) scan and rebuild dominate at hundreds of profiles).
    let _ = crate::tag_manager::TAG_MANAGER.lock().map(|tm| {
      let _ = tm.rebuild_from_profiles(&self.list_profiles().unwrap_or_default());
    });
    if let Err(e) = events::emit_empty("profiles-changed") {
      log::warn!("Warning: Failed to emit profiles-changed event: {e}");
    }

    Ok(results)
  }

  fn create_batch_many(
    &self,
    names: Vec<String>,
    req: &BatchCreateProfilesRequest,
    base_config: Option<ChromiumConfig>,
    launch_hook: Option<String>,
    existing_names: &mut std::collections::HashSet<String>,
  ) -> Vec<BatchCreateResult> {
    let mut results = Vec::with_capacity(names.len());
    for name in names {
      let lower = name.to_lowercase();
      if existing_names.contains(&lower) {
        results.push(BatchCreateResult {
          name: name.clone(),
          ok: false,
          profile: None,
          error: Some(serde_json::json!({ "code": "PROFILE_NAME_ALREADY_EXISTS" }).to_string()),
        });
        continue;
      }

      let profile_id = uuid::Uuid::new_v4();
      let profiles_dir = self.get_profiles_dir();
      let profile_uuid_dir = profiles_dir.join(profile_id.to_string());

      let created = (|| -> Result<BrowserProfile, Box<dyn std::error::Error>> {
        create_dir_all(&profile_uuid_dir)?;
        if !req.ephemeral {
          create_dir_all(profile_uuid_dir.join("profile"))?;
        }

        let profile = BrowserProfile {
          id: profile_id,
          name: name.clone(),
          browser: req.browser.clone(),
          version: req.version.clone(),
          proxy_id: req.proxy_id.clone(),
          vpn_id: req.vpn_id.clone(),
          launch_hook: launch_hook.clone(),
          process_id: None,
          last_launch: None,
          release_type: req.release_type.clone(),
          chromium_config: base_config.clone(),
          group_id: req.group_id.clone(),
          tags: Vec::new(),
          note: None,
          window_color: Some(crate::chromium_manager::derive_profile_color(&profile_id)),
          sync_mode: SyncMode::Disabled,
          encryption_salt: None,
          last_sync: None,
          host_os: Some(get_host_os()),
          ephemeral: req.ephemeral,
          extension_group_id: None,
          proxy_bypass_rules: Vec::new(),
          created_by_id: None,
          created_by_email: None,
          dns_blocklist: req.dns_blocklist.clone(),
          password_protected: false,
          clear_on_close: false,
          created_at: Some(
            std::time::SystemTime::now()
              .duration_since(std::time::UNIX_EPOCH)
              .map(|d| d.as_secs())
              .unwrap_or(0),
          ),
          updated_at: Some(crate::proxy_manager::now_secs()),
          download_dir: None,
          allow_agent_downloads: false,
          agent_auto_approve: false,
          agent_key_id: None,
          agent_id: None,
        };

        self.save_profile_raw(&profile)?;
        if !profile_uuid_dir.join("metadata.json").exists() {
          return Err("Failed to write profile metadata".into());
        }
        Ok(profile)
      })();

      match created {
        Ok(profile) => {
          existing_names.insert(lower);
          log::info!("Batch profile '{name}' created with ID: {}", profile.id);
          results.push(BatchCreateResult {
            name,
            ok: true,
            profile: Some(profile),
            error: None,
          });
        }
        Err(e) => {
          log::warn!("Batch profile '{name}' failed: {e}");
          results.push(BatchCreateResult {
            name,
            ok: false,
            profile: None,
            error: Some(e.to_string()),
          });
        }
      }
    }

    results
  }

  pub fn save_profile(&self, profile: &BrowserProfile) -> Result<(), Box<dyn std::error::Error>> {
    self.save_profile_raw(profile)?;

    // Update tag suggestions after any save
    let _ = crate::tag_manager::TAG_MANAGER.lock().map(|tm| {
      let _ = tm.rebuild_from_profiles(&self.list_profiles().unwrap_or_default());
    });

    Ok(())
  }

  /// Write `metadata.json` without the O(n) tag-index rebuild. Batch paths
  /// (e.g. `batch_create_profiles`) call this per profile and rebuild the tag
  /// index once at the end. Keeps the profile-store index fresh in the same
  /// pass.
  fn save_profile_raw(&self, profile: &BrowserProfile) -> Result<(), Box<dyn std::error::Error>> {
    let profiles_dir = self.get_profiles_dir();
    let profile_uuid_dir = profiles_dir.join(profile.id.to_string());
    let profile_file = profile_uuid_dir.join("metadata.json");

    // Ensure the UUID directory exists
    create_dir_all(&profile_uuid_dir)?;

    let json = serde_json::to_string_pretty(profile)?;
    atomic_write(&profile_file, json.as_bytes())?;

    let mtime = fs::metadata(&profile_file)
      .ok()
      .and_then(|m| m.modified().ok());
    let mut cache = PROFILE_INDEX_CACHE
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner());
    let base = cache.base(&profiles_dir);
    base.dir_mtime = fs::metadata(&profiles_dir)
      .ok()
      .and_then(|m| m.modified().ok());
    base.entries.insert(
      profile.id,
      CachedProfileEntry {
        profile: profile.clone(),
        profile_dir: profile_uuid_dir,
        mtime,
      },
    );

    Ok(())
  }

  pub fn list_profiles(&self) -> Result<Vec<BrowserProfile>, Box<dyn std::error::Error>> {
    let profiles_dir = self.get_profiles_dir();
    let mut cache = PROFILE_INDEX_CACHE
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(snapshot) = cache.snapshot_if_fresh(&profiles_dir) {
      return Ok(snapshot);
    }

    if !profiles_dir.exists() {
      cache.bases.remove(&profiles_dir);
      return Ok(vec![]);
    }

    let mut profiles = Vec::new();
    let mut entries = HashMap::new();
    for entry in fs::read_dir(&profiles_dir)? {
      let entry = entry?;
      let path = entry.path();

      // Look for UUID directories containing metadata.json
      if path.is_dir() {
        let metadata_file = path.join("metadata.json");
        if metadata_file.exists() {
          let Some(profile) = parse_metadata_file(&metadata_file) else {
            continue;
          };
          let mtime = fs::metadata(&metadata_file)
            .ok()
            .and_then(|m| m.modified().ok());
          entries.insert(
            profile.id,
            CachedProfileEntry {
              profile_dir: path,
              mtime,
              profile: profile.clone(),
            },
          );
          profiles.push(profile);
        }
      }
    }

    let base = cache.base(&profiles_dir);
    base.entries = entries;
    base.dir_mtime = fs::metadata(&profiles_dir)
      .ok()
      .and_then(|m| m.modified().ok());

    Ok(profiles)
  }

  /// Fast path by ID: serve from the index when fresh (no full scan).
  /// Falls back to a full scan and returns the first match.
  pub fn get_profile_by_id(
    &self,
    profile_id: &uuid::Uuid,
  ) -> Result<Option<BrowserProfile>, Box<dyn std::error::Error>> {
    let profiles_dir = self.get_profiles_dir();
    {
      let mut cache = PROFILE_INDEX_CACHE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
      let base = cache.base(&profiles_dir);
      if base.dir_mtime
        == fs::metadata(&profiles_dir)
          .ok()
          .and_then(|m| m.modified().ok())
      {
        if let Some(entry) = base.entries.get(profile_id) {
          if entry_is_fresh(entry) {
            return Ok(Some(entry.profile.clone()));
          }
        }
      }
    }

    // Fast path missed: rescan with the lock released (a full scan re-locks
    // the index, so this must not run while we hold it).
    Ok(
      self
        .list_profiles()?
        .into_iter()
        .find(|p| p.id == *profile_id),
    )
  }

  pub fn rename_profile(
    &self,
    _app_handle: &tauri::AppHandle,
    profile_id: &str,
    new_name: &str,
  ) -> Result<BrowserProfile, Box<dyn std::error::Error>> {
    if new_name.trim().is_empty() {
      return Err(
        serde_json::json!({ "code": "NAME_CANNOT_BE_EMPTY" })
          .to_string()
          .into(),
      );
    }

    // Check if new name already exists (case insensitive)
    let existing_profiles = self.list_profiles()?;
    if existing_profiles
      .iter()
      .any(|p| p.name.to_lowercase() == new_name.to_lowercase())
    {
      return Err(format!("Profile with name '{new_name}' already exists").into());
    }

    // Find the profile by ID
    let profile_uuid =
      uuid::Uuid::parse_str(profile_id).map_err(|_| format!("Invalid profile ID: {profile_id}"))?;
    let mut profile = existing_profiles
      .into_iter()
      .find(|p| p.id == profile_uuid)
      .ok_or_else(|| format!("Profile with ID '{profile_id}' not found"))?;

    // Update profile name (no need to move directories since we use UUID)
    profile.name = new_name.to_string();
    profile.updated_at = Some(crate::proxy_manager::now_secs());

    // Save profile with new name
    self.save_profile(&profile)?;

    crate::sync::queue_profile_sync_if_eligible(&profile);

    // Keep tag suggestions up to date after name change (rebuild from all profiles)
    let _ = crate::tag_manager::TAG_MANAGER.lock().map(|tm| {
      let _ = tm.rebuild_from_profiles(&self.list_profiles().unwrap_or_default());
    });

    // Emit profile rename event
    if let Err(e) = events::emit_empty("profiles-changed") {
      log::warn!("Warning: Failed to emit profiles-changed event: {e}");
    }

    Ok(profile)
  }

  pub fn delete_profile(
    &self,
    app_handle: &tauri::AppHandle,
    profile_id: &str,
  ) -> Result<(), Box<dyn std::error::Error>> {
    log::info!("Attempting to delete profile with ID: {profile_id}");

    // Find the profile by ID
    let profile_uuid =
      uuid::Uuid::parse_str(profile_id).map_err(|_| format!("Invalid profile ID: {profile_id}"))?;
    let profiles = self.list_profiles()?;
    let profile = profiles
      .into_iter()
      .find(|p| p.id == profile_uuid)
      .ok_or_else(|| format!("Profile with ID '{profile_id}' not found"))?;

    // Check if browser is running (cross-OS profiles can't be running locally)
    if profile.process_id.is_some() && !profile.is_cross_os() {
      return Err(
        "Cannot delete profile while browser is running. Please stop the browser first.".into(),
      );
    }

    // Remember sync mode before deleting local files
    let was_sync_enabled = profile.is_sync_enabled();

    let profiles_dir = self.get_profiles_dir();
    let profile_uuid_dir = profiles_dir.join(profile.id.to_string());

    // Delete the entire UUID directory (contains both metadata.json and profile data)
    if profile_uuid_dir.exists() {
      log::info!("Deleting profile directory: {}", profile_uuid_dir.display());
      fs::remove_dir_all(&profile_uuid_dir)?;
      log::info!("Profile directory deleted successfully");
    }

    // Verify deletion was successful
    if profile_uuid_dir.exists() {
      return Err(format!("Failed to completely delete profile '{}'", profile.name).into());
    }

    log::info!(
      "Profile '{}' (ID: {}) deleted successfully",
      profile.name,
      profile_id
    );

    // If sync was enabled, also delete from S3
    if was_sync_enabled {
      let profile_id_owned = profile_id.to_string();
      let app_handle_clone = app_handle.clone();
      tauri::async_runtime::spawn(async move {
        match crate::sync::SyncEngine::create_from_settings(&app_handle_clone).await {
          Ok(engine) => {
            if let Err(e) = engine.delete_profile(&profile_id_owned).await {
              log::warn!(
                "Failed to delete profile {} from sync: {}",
                profile_id_owned,
                e
              );
            } else {
              log::info!("Profile {} deleted from S3 sync storage", profile_id_owned);
            }
          }
          Err(e) => {
            log::debug!("Sync not configured, skipping remote deletion: {}", e);
          }
        }
      });
    }

    // Rebuild tag suggestions after deletion
    let _ = crate::tag_manager::TAG_MANAGER.lock().map(|tm| {
      let _ = tm.rebuild_from_profiles(&self.list_profiles().unwrap_or_default());
    });

    // Always perform cleanup after profile deletion to remove unused binaries
    if let Err(e) = DownloadedBrowsersRegistry::instance().cleanup_unused_binaries() {
      log::warn!("Warning: Failed to cleanup unused binaries after profile deletion: {e}");
    }

    // Emit profile deletion event
    if let Err(e) = events::emit_empty("profiles-changed") {
      log::warn!("Warning: Failed to emit profiles-changed event: {e}");
    }

    Ok(())
  }

  /// Delete a profile from the local filesystem only, without triggering remote sync deletion.
  /// Used when a profile was deleted on another device and the local copy should be cleaned up.
  pub fn delete_profile_local_only(
    &self,
    profile_id: &str,
  ) -> Result<(), Box<dyn std::error::Error>> {
    let profiles_dir = self.get_profiles_dir();
    let profile_dir = profiles_dir.join(profile_id);
    if profile_dir.exists() {
      fs::remove_dir_all(&profile_dir)?;
      log::info!("Deleted local profile {} (tombstoned remotely)", profile_id);
    }

    if let Err(e) = crate::downloaded_browsers_registry::DownloadedBrowsersRegistry::instance()
      .cleanup_unused_binaries()
    {
      log::warn!("Failed to cleanup binaries after tombstone deletion: {e}");
    }

    let _ = crate::events::emit_empty("profiles-changed");
    Ok(())
  }

  pub fn update_profile_version(
    &self,
    _app_handle: &tauri::AppHandle,
    profile_id: &str,
    version: &str,
  ) -> Result<BrowserProfile, Box<dyn std::error::Error>> {
    // Find the profile by ID
    let profile_uuid =
      uuid::Uuid::parse_str(profile_id).map_err(|_| format!("Invalid profile ID: {profile_id}"))?;
    let profiles = self.list_profiles()?;
    let mut profile = profiles
      .into_iter()
      .find(|p| p.id == profile_uuid)
      .ok_or_else(|| format!("Profile with ID '{profile_id}' not found"))?;

    // Check if the browser is currently running
    if profile.process_id.is_some() {
      return Err(
        "Cannot update version while browser is running. Please stop the browser first.".into(),
      );
    }

    // Verify the new version is downloaded
    let browser_type = BrowserType::from_str(&profile.browser)
      .map_err(|_| format!("Invalid browser type: {}", profile.browser))?;
    let browser = create_browser(browser_type.clone());
    let binaries_dir = self.get_binaries_dir();

    if !browser.is_version_downloaded(version, &binaries_dir) {
      return Err(format!("Browser version {version} is not downloaded").into());
    }

    // Update version
    profile.version = version.to_string();

    profile.release_type = "stable".to_string();

    // Save the updated profile
    self.save_profile(&profile)?;

    // Emit profile update event
    if let Err(e) = events::emit_empty("profiles-changed") {
      log::warn!("Warning: Failed to emit profiles-changed event: {e}");
    }

    Ok(profile)
  }

  pub fn assign_profiles_to_group(
    &self,
    _app_handle: &tauri::AppHandle,
    profile_ids: Vec<String>,
    group_id: Option<String>,
  ) -> Result<(), Box<dyn std::error::Error>> {
    let profiles = self.list_profiles()?;

    for profile_id in profile_ids {
      let profile_uuid = uuid::Uuid::parse_str(&profile_id)
        .map_err(|_| format!("Invalid profile ID: {profile_id}"))?;
      let mut profile = profiles
        .iter()
        .find(|p| p.id == profile_uuid)
        .ok_or_else(|| format!("Profile with ID '{profile_id}' not found"))?
        .clone();

      // Check if browser is running
      if profile.process_id.is_some() {
        return Err(format!(
          "Cannot modify group for profile '{}' while browser is running. Please stop the browser first.", profile.name
        ).into());
      }

      profile.group_id = group_id.clone();
      profile.updated_at = Some(crate::proxy_manager::now_secs());
      self.save_profile(&profile)?;

      crate::sync::queue_profile_sync_if_eligible(&profile);

      // Auto-enable sync for new group if profile has sync enabled
      if profile.is_sync_enabled() {
        if let Some(ref new_group_id) = group_id {
          let group_id_clone = new_group_id.clone();
          tauri::async_runtime::spawn(async move {
            let _ = crate::sync::enable_group_sync_if_needed(&group_id_clone).await;
            if let Some(scheduler) = crate::sync::get_global_scheduler() {
              scheduler.queue_group_sync(group_id_clone).await;
            }
          });
        }
      }
    }

    // Rebuild tag suggestions after group changes just in case
    let _ = crate::tag_manager::TAG_MANAGER.lock().map(|tm| {
      let _ = tm.rebuild_from_profiles(&self.list_profiles().unwrap_or_default());
    });

    // Emit profile group assignment event
    if let Err(e) = events::emit_empty("profiles-changed") {
      log::warn!("Warning: Failed to emit profiles-changed event: {e}");
    }

    Ok(())
  }

  pub fn update_profile_tags(
    &self,
    _app_handle: &tauri::AppHandle,
    profile_id: &str,
    tags: Vec<String>,
  ) -> Result<BrowserProfile, Box<dyn std::error::Error>> {
    // Find the profile by ID
    let profile_uuid =
      uuid::Uuid::parse_str(profile_id).map_err(|_| format!("Invalid profile ID: {profile_id}"))?;
    let profiles = self.list_profiles()?;
    let mut profile = profiles
      .into_iter()
      .find(|p| p.id == profile_uuid)
      .ok_or_else(|| format!("Profile with ID '{profile_id}' not found"))?;

    let mut seen = std::collections::HashSet::new();
    let mut deduped: Vec<String> = Vec::with_capacity(tags.len());
    for t in tags.into_iter() {
      if seen.insert(t.clone()) {
        deduped.push(t);
      }
    }
    profile.tags = deduped;
    profile.updated_at = Some(crate::proxy_manager::now_secs());

    // Save profile
    self.save_profile(&profile)?;

    crate::sync::queue_profile_sync_if_eligible(&profile);

    // Update global tag suggestions from all profiles
    let _ = crate::tag_manager::TAG_MANAGER.lock().map(|tm| {
      let _ = tm.rebuild_from_profiles(&self.list_profiles().unwrap_or_default());
    });

    // Emit profile tags update event
    if let Err(e) = events::emit_empty("profiles-changed") {
      log::warn!("Warning: Failed to emit profiles-changed event: {e}");
    }

    Ok(profile)
  }

  pub fn update_profile_note(
    &self,
    _app_handle: &tauri::AppHandle,
    profile_id: &str,
    note: Option<String>,
  ) -> Result<BrowserProfile, Box<dyn std::error::Error>> {
    // Find the profile by ID
    let profile_uuid =
      uuid::Uuid::parse_str(profile_id).map_err(|_| format!("Invalid profile ID: {profile_id}"))?;
    let profiles = self.list_profiles()?;
    let mut profile = profiles
      .into_iter()
      .find(|p| p.id == profile_uuid)
      .ok_or_else(|| format!("Profile with ID '{profile_id}' not found"))?;

    // Update note (trim whitespace, set to None if empty)
    profile.note = note.map(|n| n.trim().to_string()).filter(|n| !n.is_empty());
    profile.updated_at = Some(crate::proxy_manager::now_secs());

    // Save profile
    self.save_profile(&profile)?;

    crate::sync::queue_profile_sync_if_eligible(&profile);

    // Emit profile note update event
    if let Err(e) = events::emit_empty("profiles-changed") {
      log::warn!("Warning: Failed to emit profiles-changed event: {e}");
    }

    Ok(profile)
  }

  pub fn update_profile_clear_on_close(
    &self,
    _app_handle: &tauri::AppHandle,
    profile_id: &str,
    clear_on_close: bool,
  ) -> Result<BrowserProfile, Box<dyn std::error::Error>> {
    let profile_uuid =
      uuid::Uuid::parse_str(profile_id).map_err(|_| format!("Invalid profile ID: {profile_id}"))?;
    let profiles = self.list_profiles()?;
    let mut profile = profiles
      .into_iter()
      .find(|p| p.id == profile_uuid)
      .ok_or_else(|| format!("Profile with ID '{profile_id}' not found"))?;

    // Ephemeral profiles are already wiped on close; password-protected ones
    // re-encrypt and never persist plaintext — the flag is meaningless there.
    if clear_on_close && (profile.ephemeral || profile.password_protected) {
      return Err(
        serde_json::json!({ "code": "CLEAR_ON_CLOSE_UNAVAILABLE" })
          .to_string()
          .into(),
      );
    }

    profile.clear_on_close = clear_on_close;
    profile.updated_at = Some(crate::proxy_manager::now_secs());

    self.save_profile(&profile)?;

    crate::sync::queue_profile_sync_if_eligible(&profile);

    if let Err(e) = events::emit_empty("profiles-changed") {
      log::warn!("Warning: Failed to emit profiles-changed event: {e}");
    }

    Ok(profile)
  }

  pub fn update_profile_download_dir(
    &self,
    _app_handle: &tauri::AppHandle,
    profile_id: &str,
    download_dir: Option<String>,
  ) -> Result<BrowserProfile, Box<dyn std::error::Error>> {
    let profile_uuid =
      uuid::Uuid::parse_str(profile_id).map_err(|_| format!("Invalid profile ID: {profile_id}"))?;
    let profiles = self.list_profiles()?;
    let mut profile = profiles
      .into_iter()
      .find(|p| p.id == profile_uuid)
      .ok_or_else(|| format!("Profile with ID '{profile_id}' not found"))?;

    // Validate before persisting: absolute + creatable, empty clears back to
    // the per-profile default.
    profile.download_dir = match download_dir {
      Some(raw) if !raw.trim().is_empty() => Some(
        crate::browser_downloads::validate_configured_dir(&raw)
          .map(|p| p.to_string_lossy().to_string())
          .map_err(|e| format!("Invalid download folder: {e}"))?,
      ),
      _ => None,
    };
    profile.updated_at = Some(crate::proxy_manager::now_secs());

    self.save_profile(&profile)?;

    crate::sync::queue_profile_sync_if_eligible(&profile);

    if let Err(e) = events::emit_empty("profiles-changed") {
      log::warn!("Warning: Failed to emit profiles-changed event: {e}");
    }

    Ok(profile)
  }

  pub fn update_profile_allow_agent_downloads(
    &self,
    _app_handle: &tauri::AppHandle,
    profile_id: &str,
    allow: bool,
  ) -> Result<BrowserProfile, Box<dyn std::error::Error>> {
    let profile_uuid =
      uuid::Uuid::parse_str(profile_id).map_err(|_| format!("Invalid profile ID: {profile_id}"))?;
    let profiles = self.list_profiles()?;
    let mut profile = profiles
      .into_iter()
      .find(|p| p.id == profile_uuid)
      .ok_or_else(|| format!("Profile with ID '{profile_id}' not found"))?;

    profile.allow_agent_downloads = allow;
    profile.updated_at = Some(crate::proxy_manager::now_secs());

    self.save_profile(&profile)?;

    crate::sync::queue_profile_sync_if_eligible(&profile);

    if let Err(e) = events::emit_empty("profiles-changed") {
      log::warn!("Warning: Failed to emit profiles-changed event: {e}");
    }

    Ok(profile)
  }

  pub fn update_profile_agent_auto_approve(
    &self,
    _app_handle: &tauri::AppHandle,
    profile_id: &str,
    auto_approve: bool,
  ) -> Result<BrowserProfile, Box<dyn std::error::Error>> {
    let profile_uuid =
      uuid::Uuid::parse_str(profile_id).map_err(|_| format!("Invalid profile ID: {profile_id}"))?;
    let profiles = self.list_profiles()?;
    let mut profile = profiles
      .into_iter()
      .find(|p| p.id == profile_uuid)
      .ok_or_else(|| format!("Profile with ID '{profile_id}' not found"))?;

    profile.agent_auto_approve = auto_approve;
    profile.updated_at = Some(crate::proxy_manager::now_secs());

    self.save_profile(&profile)?;

    crate::sync::queue_profile_sync_if_eligible(&profile);

    if let Err(e) = events::emit_empty("profiles-changed") {
      log::warn!("Warning: Failed to emit profiles-changed event: {e}");
    }

    Ok(profile)
  }

  pub fn update_profile_agent_pair(
    &self,
    _app_handle: &tauri::AppHandle,
    profile_id: &str,
    key_id: Option<String>,
    agent_id: Option<String>,
  ) -> Result<BrowserProfile, Box<dyn std::error::Error>> {
    let profile_uuid =
      uuid::Uuid::parse_str(profile_id).map_err(|_| format!("Invalid profile ID: {profile_id}"))?;
    let profiles = self.list_profiles()?;
    let mut profile = profiles
      .into_iter()
      .find(|p| p.id == profile_uuid)
      .ok_or_else(|| format!("Profile with ID '{profile_id}' not found"))?;

    profile.agent_key_id = key_id.and_then(|k| {
      let trimmed = k.trim().to_string();
      if trimmed.is_empty() {
        None
      } else {
        Some(trimmed)
      }
    });
    profile.agent_id = agent_id.and_then(|a| {
      let trimmed = a.trim().to_string();
      if trimmed.is_empty() {
        None
      } else {
        Some(trimmed)
      }
    });
    profile.updated_at = Some(crate::proxy_manager::now_secs());

    self.save_profile(&profile)?;

    crate::sync::queue_profile_sync_if_eligible(&profile);

    if let Err(e) = events::emit_empty("profiles-changed") {
      log::warn!("Warning: Failed to emit profiles-changed event: {e}");
    }

    Ok(profile)
  }

  pub fn update_profile_window_color(
    &self,
    _app_handle: &tauri::AppHandle,
    profile_id: &str,
    window_color: Option<String>,
  ) -> Result<BrowserProfile, Box<dyn std::error::Error>> {
    let profile_uuid =
      uuid::Uuid::parse_str(profile_id).map_err(|_| format!("Invalid profile ID: {profile_id}"))?;
    let profiles = self.list_profiles()?;
    let mut profile = profiles
      .into_iter()
      .find(|p| p.id == profile_uuid)
      .ok_or_else(|| format!("Profile with ID '{profile_id}' not found"))?;

    // Normalize to lowercase #RRGGBB, or clear (None) for an invalid/empty value
    // so it reverts to the auto id-derived color at next launch.
    profile.window_color = window_color.and_then(|c| {
      let hex = c.trim().trim_start_matches('#');
      (hex.len() == 6 && hex.chars().all(|ch| ch.is_ascii_hexdigit()))
        .then(|| format!("#{}", hex.to_lowercase()))
    });
    profile.updated_at = Some(crate::proxy_manager::now_secs());

    self.save_profile(&profile)?;
    crate::sync::queue_profile_sync_if_eligible(&profile);
    if let Err(e) = events::emit_empty("profiles-changed") {
      log::warn!("Warning: Failed to emit profiles-changed event: {e}");
    }

    Ok(profile)
  }

  pub fn update_profile_launch_hook(
    &self,
    _app_handle: &tauri::AppHandle,
    profile_id: &str,
    launch_hook: Option<String>,
  ) -> Result<BrowserProfile, Box<dyn std::error::Error>> {
    let profile_uuid =
      uuid::Uuid::parse_str(profile_id).map_err(|_| format!("Invalid profile ID: {profile_id}"))?;
    let profiles = self.list_profiles()?;
    let mut profile = profiles
      .into_iter()
      .find(|p| p.id == profile_uuid)
      .ok_or_else(|| format!("Profile with ID '{profile_id}' not found"))?;

    profile.launch_hook = Self::normalize_launch_hook(launch_hook)?;
    profile.updated_at = Some(crate::proxy_manager::now_secs());

    self.save_profile(&profile)?;

    crate::sync::queue_profile_sync_if_eligible(&profile);

    if let Err(e) = events::emit("profile-updated", &profile) {
      log::warn!("Warning: Failed to emit profile update event: {e}");
    }

    if let Err(e) = events::emit_empty("profiles-changed") {
      log::warn!("Warning: Failed to emit profiles-changed event: {e}");
    }

    Ok(profile)
  }

  pub fn update_profile_proxy_bypass_rules(
    &self,
    _app_handle: &tauri::AppHandle,
    profile_id: &str,
    rules: Vec<String>,
  ) -> Result<BrowserProfile, Box<dyn std::error::Error>> {
    let profile_uuid =
      uuid::Uuid::parse_str(profile_id).map_err(|_| format!("Invalid profile ID: {profile_id}"))?;
    let profiles = self.list_profiles()?;
    let mut profile = profiles
      .into_iter()
      .find(|p| p.id == profile_uuid)
      .ok_or_else(|| format!("Profile with ID '{profile_id}' not found"))?;

    profile.proxy_bypass_rules = rules;
    profile.updated_at = Some(crate::proxy_manager::now_secs());

    self.save_profile(&profile)?;

    crate::sync::queue_profile_sync_if_eligible(&profile);

    if let Err(e) = events::emit_empty("profiles-changed") {
      log::warn!("Warning: Failed to emit profiles-changed event: {e}");
    }

    Ok(profile)
  }

  pub fn update_profile_dns_blocklist(
    &self,
    profile_id: &str,
    dns_blocklist: Option<String>,
  ) -> Result<BrowserProfile, Box<dyn std::error::Error>> {
    let profile_uuid =
      uuid::Uuid::parse_str(profile_id).map_err(|_| format!("Invalid profile ID: {profile_id}"))?;
    let profiles = self.list_profiles()?;
    let mut profile = profiles
      .into_iter()
      .find(|p| p.id == profile_uuid)
      .ok_or_else(|| format!("Profile with ID '{profile_id}' not found"))?;

    profile.dns_blocklist = dns_blocklist;
    profile.updated_at = Some(crate::proxy_manager::now_secs());

    self.save_profile(&profile)?;

    crate::sync::queue_profile_sync_if_eligible(&profile);

    if let Err(e) = events::emit_empty("profiles-changed") {
      log::warn!("Warning: Failed to emit profiles-changed event: {e}");
    }

    Ok(profile)
  }

  pub fn delete_multiple_profiles(
    &self,
    app_handle: &tauri::AppHandle,
    profile_ids: Vec<String>,
  ) -> Result<(), Box<dyn std::error::Error>> {
    let profiles = self.list_profiles()?;
    let mut sync_enabled_ids: Vec<String> = Vec::new();

    for profile_id in profile_ids {
      let profile_uuid = uuid::Uuid::parse_str(&profile_id)
        .map_err(|_| format!("Invalid profile ID: {profile_id}"))?;
      let profile = profiles
        .iter()
        .find(|p| p.id == profile_uuid)
        .ok_or_else(|| format!("Profile with ID '{profile_id}' not found"))?;

      // Check if browser is running (cross-OS profiles can't be running locally)
      if profile.process_id.is_some() && !profile.is_cross_os() {
        return Err(
          format!(
            "Cannot delete profile '{}' while browser is running. Please stop the browser first.",
            profile.name
          )
          .into(),
        );
      }

      // Track sync-enabled profiles for remote deletion
      if profile.is_sync_enabled() {
        sync_enabled_ids.push(profile_id.clone());
      }

      // Delete the profile
      let profiles_dir = self.get_profiles_dir();
      let profile_uuid_dir = profiles_dir.join(profile.id.to_string());

      if profile_uuid_dir.exists() {
        std::fs::remove_dir_all(&profile_uuid_dir)?;
      }
    }

    // Delete sync-enabled profiles from S3
    if !sync_enabled_ids.is_empty() {
      let app_handle_clone = app_handle.clone();
      tauri::async_runtime::spawn(async move {
        if let Ok(engine) = crate::sync::SyncEngine::create_from_settings(&app_handle_clone).await {
          for profile_id in sync_enabled_ids {
            if let Err(e) = engine.delete_profile(&profile_id).await {
              log::warn!("Failed to delete profile {} from sync: {}", profile_id, e);
            }
          }
        }
      });
    }

    // Emit profile deletion event
    if let Err(e) = events::emit_empty("profiles-changed") {
      log::warn!("Warning: Failed to emit profiles-changed event: {e}");
    }

    Ok(())
  }

  fn generate_clone_name(&self, original_name: &str) -> Result<String, Box<dyn std::error::Error>> {
    let profiles = self.list_profiles()?;
    let existing_names: std::collections::HashSet<String> =
      profiles.iter().map(|p| p.name.clone()).collect();

    let candidate = format!("{original_name} (Copy)");
    if !existing_names.contains(&candidate) {
      return Ok(candidate);
    }

    for i in 2.. {
      let candidate = format!("{original_name} (Copy {i})");
      if !existing_names.contains(&candidate) {
        return Ok(candidate);
      }
    }

    unreachable!()
  }

  pub fn clone_profile(
    &self,
    profile_id: &str,
    custom_name: Option<String>,
  ) -> Result<BrowserProfile, Box<dyn std::error::Error>> {
    let profile_uuid =
      uuid::Uuid::parse_str(profile_id).map_err(|_| format!("Invalid profile ID: {profile_id}"))?;
    let profiles = self.list_profiles()?;
    let source = profiles
      .into_iter()
      .find(|p| p.id == profile_uuid)
      .ok_or_else(|| format!("Profile with ID '{profile_id}' not found"))?;

    if source.process_id.is_some() {
      return Err(
        "Cannot clone profile while browser is running. Please stop the browser first.".into(),
      );
    }

    let new_id = uuid::Uuid::new_v4();
    let clone_name = match custom_name {
      Some(name) if !name.trim().is_empty() => name.trim().to_string(),
      _ => self.generate_clone_name(&source.name)?,
    };

    let profiles_dir = self.get_profiles_dir();
    let source_dir = profiles_dir.join(source.id.to_string());
    let dest_dir = profiles_dir.join(new_id.to_string());

    if source_dir.exists() {
      crate::profile_importer::ProfileImporter::copy_directory_recursive(&source_dir, &dest_dir)?;
    } else {
      fs::create_dir_all(&dest_dir)?;
    }

    let mut new_profile = BrowserProfile {
      id: new_id,
      name: clone_name,
      browser: source.browser,
      version: source.version,
      proxy_id: source.proxy_id,
      vpn_id: source.vpn_id,
      launch_hook: source.launch_hook,
      process_id: None,
      last_launch: None,
      release_type: source.release_type,
      chromium_config: source.chromium_config,
      group_id: source.group_id,
      tags: source.tags,
      note: source.note,
      window_color: source.window_color,
      sync_mode: SyncMode::Disabled,
      encryption_salt: None,
      last_sync: None,
      host_os: Some(get_host_os()),
      ephemeral: false,
      extension_group_id: source.extension_group_id,
      proxy_bypass_rules: source.proxy_bypass_rules,
      created_by_id: None,
      created_by_email: None,
      dns_blocklist: source.dns_blocklist,
      password_protected: false,
      clear_on_close: false,
      created_at: Some(
        std::time::SystemTime::now()
          .duration_since(std::time::UNIX_EPOCH)
          .map(|d| d.as_secs())
          .unwrap_or(0),
      ),
      updated_at: Some(crate::proxy_manager::now_secs()),
      download_dir: source.download_dir,
      allow_agent_downloads: source.allow_agent_downloads,
      agent_auto_approve: source.agent_auto_approve,
      agent_key_id: source.agent_key_id,
      agent_id: source.agent_id,
    };

    // Duckling: a clone must NOT be linkable to its source. The source
    // chromium_config embeds the persisted fingerprint JSON (including the
    // canvas_noise_seed), so copying it verbatim makes the clone emit
    // BYTE-IDENTICAL canvas/WebGL/audio readback hashes and identical device
    // signals as the source — trivially linkable if both run concurrently. Clear
    // the fingerprint so the launch path mints a fresh one (a new
    // canvas_noise_seed via RandBytes + an independent device fingerprint),
    // exactly as create_profile does when fingerprint.is_none(). NOTE: the
    // user-data-dir copy above still duplicates cookies/localStorage/TLS state —
    // a separate storage-linkage vector the user must clear if they want full
    // isolation between a clone and its source.
    if let Some(cfg) = new_profile.chromium_config.as_mut() {
      cfg.fingerprint = None;
    }

    self.save_profile(&new_profile)?;

    if let Err(e) = events::emit_empty("profiles-changed") {
      log::warn!("Warning: Failed to emit profiles-changed event: {e}");
    }

    Ok(new_profile)
  }

  pub async fn update_chromium_config(
    &self,
    app_handle: tauri::AppHandle,
    profile_id: &str,
    config: ChromiumConfig,
  ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Find the profile by ID
    let profile_uuid = uuid::Uuid::parse_str(profile_id).map_err(
      |_| -> Box<dyn std::error::Error + Send + Sync> {
        format!("Invalid profile ID: {profile_id}").into()
      },
    )?;
    let profiles =
      self
        .list_profiles()
        .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
          format!("Failed to list profiles: {e}").into()
        })?;
    let mut profile = profiles
      .into_iter()
      .find(|p| p.id == profile_uuid)
      .ok_or_else(|| -> Box<dyn std::error::Error + Send + Sync> {
        format!("Profile with ID '{profile_id}' not found").into()
      })?;

    // Check if the browser is currently running using the comprehensive status check
    let is_running = self
      .check_browser_status(app_handle.clone(), &profile)
      .await?;

    if is_running {
      return Err(
        "Cannot update browser configuration while browser is running. Please stop the browser first.".into(),
      );
    }

    // Update the browser configuration
    profile.chromium_config = Some(config);

    // Save the updated profile
    self
      .save_profile(&profile)
      .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
        format!("Failed to save profile: {e}").into()
      })?;

    crate::sync::queue_profile_sync_if_eligible(&profile);

    log::info!(
      "Browser configuration updated for profile '{}' (ID: {}).",
      profile.name,
      profile_id
    );

    // Emit profile config update event
    if let Err(e) = events::emit_empty("profiles-changed") {
      log::warn!("Warning: Failed to emit profiles-changed event: {e}");
    }

    Ok(())
  }

  pub async fn update_profile_proxy(
    &self,
    _app_handle: tauri::AppHandle,
    profile_id: &str,
    proxy_id: Option<String>,
  ) -> Result<BrowserProfile, Box<dyn std::error::Error + Send + Sync>> {
    // Find the profile by ID
    let profile_uuid = uuid::Uuid::parse_str(profile_id).map_err(
      |_| -> Box<dyn std::error::Error + Send + Sync> {
        format!("Invalid profile ID: {profile_id}").into()
      },
    )?;
    let profiles =
      self
        .list_profiles()
        .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
          format!("Failed to list profiles: {e}").into()
        })?;

    let mut profile = profiles
      .into_iter()
      .find(|p| p.id == profile_uuid)
      .ok_or_else(|| -> Box<dyn std::error::Error + Send + Sync> {
        format!("Profile with ID '{profile_id}' not found").into()
      })?;

    // Remember old proxy_id for cleanup (not used yet, but may be needed for cleanup)
    let _old_proxy_id = profile.proxy_id.clone();

    // Update proxy settings and clear VPN (mutual exclusion)
    profile.proxy_id = proxy_id.clone();
    profile.vpn_id = None;
    profile.updated_at = Some(crate::proxy_manager::now_secs());

    // Save the updated profile
    self
      .save_profile(&profile)
      .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
        format!("Failed to save profile: {e}").into()
      })?;

    crate::sync::queue_profile_sync_if_eligible(&profile);

    // Auto-enable sync for new proxy if profile has sync enabled
    if profile.is_sync_enabled() {
      if let Some(ref new_proxy_id) = proxy_id {
        let _ = crate::sync::enable_proxy_sync_if_needed(new_proxy_id).await;
        if let Some(scheduler) = crate::sync::get_global_scheduler() {
          scheduler.queue_proxy_sync(new_proxy_id.clone()).await;
        }
      }
    }

    // Emit profile update event so frontend UIs can refresh immediately (e.g. proxy manager)
    if let Err(e) = events::emit("profile-updated", &profile) {
      log::warn!("Warning: Failed to emit profile update event: {e}");
    }

    // Emit general profiles changed event for profile list updates
    if let Err(e) = events::emit_empty("profiles-changed") {
      log::warn!("Warning: Failed to emit profiles-changed event: {e}");
    }

    Ok(profile)
  }

  pub async fn update_profile_vpn(
    &self,
    _app_handle: tauri::AppHandle,
    profile_id: &str,
    vpn_id: Option<String>,
  ) -> Result<BrowserProfile, Box<dyn std::error::Error + Send + Sync>> {
    let profile_uuid = uuid::Uuid::parse_str(profile_id).map_err(
      |_| -> Box<dyn std::error::Error + Send + Sync> {
        format!("Invalid profile ID: {profile_id}").into()
      },
    )?;
    let profiles =
      self
        .list_profiles()
        .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
          format!("Failed to list profiles: {e}").into()
        })?;

    let mut profile = profiles
      .into_iter()
      .find(|p| p.id == profile_uuid)
      .ok_or_else(|| -> Box<dyn std::error::Error + Send + Sync> {
        format!("Profile with ID '{profile_id}' not found").into()
      })?;

    // Update VPN and clear proxy (mutual exclusion)
    profile.vpn_id = vpn_id.clone();
    profile.proxy_id = None;
    profile.updated_at = Some(crate::proxy_manager::now_secs());

    self
      .save_profile(&profile)
      .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
        format!("Failed to save profile: {e}").into()
      })?;

    crate::sync::queue_profile_sync_if_eligible(&profile);

    // Auto-enable sync for the new VPN if profile has sync enabled.
    if profile.is_sync_enabled() {
      if let Some(ref new_vpn_id) = vpn_id {
        let _ = crate::sync::enable_vpn_sync_if_needed(new_vpn_id).await;
        if let Some(scheduler) = crate::sync::get_global_scheduler() {
          scheduler.queue_vpn_sync(new_vpn_id.clone()).await;
        }
      }
    }

    if let Err(e) = events::emit("profile-updated", &profile) {
      log::warn!("Warning: Failed to emit profile update event: {e}");
    }

    if let Err(e) = events::emit_empty("profiles-changed") {
      log::warn!("Warning: Failed to emit profiles-changed event: {e}");
    }

    Ok(profile)
  }

  pub fn update_profile_extension_group(
    &self,
    profile_id: &str,
    extension_group_id: Option<String>,
  ) -> Result<BrowserProfile, Box<dyn std::error::Error>> {
    let profile_uuid =
      uuid::Uuid::parse_str(profile_id).map_err(|_| format!("Invalid profile ID: {profile_id}"))?;
    let profiles = self.list_profiles()?;
    let mut profile = profiles
      .into_iter()
      .find(|p| p.id == profile_uuid)
      .ok_or_else(|| format!("Profile with ID '{profile_id}' not found"))?;

    profile.extension_group_id = extension_group_id.clone();
    profile.updated_at = Some(crate::proxy_manager::now_secs());
    self.save_profile(&profile)?;

    crate::sync::queue_profile_sync_if_eligible(&profile);

    // Auto-enable sync for the new extension group if profile has sync
    // enabled. The helper is sync internally; we fire-and-forget through
    // the async runtime so any I/O doesn't block this caller.
    if profile.is_sync_enabled() {
      if let Some(new_group_id) = extension_group_id {
        tauri::async_runtime::spawn(async move {
          let _ = crate::sync::enable_extension_group_sync_if_needed(&new_group_id).await;
          if let Some(scheduler) = crate::sync::get_global_scheduler() {
            scheduler.queue_extension_group_sync(new_group_id).await;
          }
        });
      }
    }

    if let Err(e) = events::emit("profile-updated", &profile) {
      log::warn!("Failed to emit profile update event: {e}");
    }
    if let Err(e) = events::emit_empty("profiles-changed") {
      log::warn!("Failed to emit profiles-changed event: {e}");
    }

    Ok(profile)
  }

  pub async fn check_browser_status(
    &self,
    app_handle: tauri::AppHandle,
    profile: &BrowserProfile,
  ) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    // Handle Chromium profiles using ChromiumManager-based status checking
    if profile.browser == "chromium" {
      return self.check_chromium_status(&app_handle, profile).await;
    }

    // For non-Chromium browsers, use the existing PID-based logic
    let inner_profile = profile.clone();
    let system = System::new_with_specifics(
      RefreshKind::nothing().with_processes(ProcessRefreshKind::everything()),
    );
    let mut is_running = false;
    let mut found_pid: Option<u32> = None;

    // First check if the stored PID is still valid
    if let Some(pid) = profile.process_id {
      if let Some(process) = system.process(Pid::from(pid as usize)) {
        let cmd = process.cmd();
        // Verify this process is actually our browser with the correct profile
        let profiles_dir = self.get_profiles_dir();
        let profile_data_path = profile.get_profile_data_path(&profiles_dir);
        let profile_data_path_str = profile_data_path.to_string_lossy();
        let profile_path_match = cmd.iter().any(|s| {
          let arg = s.to_str().unwrap_or("");
          // Match the Chromium --user-data-dir flag or an exact profile path argument
          arg.contains(&format!("--user-data-dir={profile_data_path_str}"))
            || arg == profile_data_path_str
        });

        if profile_path_match {
          is_running = true;
          found_pid = Some(pid);
        }
      }
    }

    // If we didn't find the browser with the stored PID, search all processes
    if !is_running {
      for (pid, process) in system.processes() {
        let cmd = process.cmd();
        if cmd.len() >= 2 {
          // Check if this is the right browser executable first
          let exe_name = process.name().to_string_lossy().to_lowercase();
          let is_correct_browser = match profile.browser.as_str() {
            "chromium" => {
              exe_name.contains("chromium")
                || exe_name.contains("chromium")
                || exe_name.contains("chrome")
            }
            _ => false,
          };

          if !is_correct_browser {
            continue;
          }

          // Check for profile path match
          let profiles_dir = self.get_profiles_dir();
          let profile_data_path = profile.get_profile_data_path(&profiles_dir);
          let profile_data_path_str = profile_data_path.to_string_lossy();
          let profile_path_match = cmd.iter().any(|s| {
            let arg = s.to_str().unwrap_or("");
            // Match the Chromium --user-data-dir flag or an exact profile path argument
            arg.contains(&format!("--user-data-dir={profile_data_path_str}"))
              || arg == profile_data_path_str
          });

          if profile_path_match {
            // Found a matching process
            found_pid = Some(pid.as_u32());
            is_running = true;
            log::info!(
              "Found browser process with PID: {} for profile: {}",
              pid.as_u32(),
              profile.name
            );
            break;
          }
        }
      }
    }

    // Only persist status changes if the profile metadata still exists on disk
    let profiles_dir = self.get_profiles_dir();
    let profile_uuid_dir = profiles_dir.join(profile.id.to_string());
    let metadata_file = profile_uuid_dir.join("metadata.json");
    let metadata_exists = metadata_file.exists();

    if metadata_exists {
      // Load the latest profile from disk to avoid overwriting fields like proxy_id
      let latest_profile: BrowserProfile = match std::fs::read_to_string(&metadata_file)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
      {
        Some(p) => p,
        None => inner_profile.clone(),
      };

      let mut merged = latest_profile.clone();
      let mut detected_stop = false;

      if let Some(pid) = found_pid {
        if merged.process_id != Some(pid) {
          let old_pid = merged.process_id;
          merged.process_id = Some(pid);
          if let Err(e) = self.save_profile(&merged) {
            log::warn!("Warning: Failed to update profile with new PID: {e}");
          }
          if let Some(prev) = old_pid {
            let _ = crate::proxy_manager::PROXY_MANAGER.update_proxy_pid(prev, pid);
          }
        }
      } else if merged.process_id.is_some() {
        // Clear the PID if no process found
        merged.process_id = None;
        if let Err(e) = self.save_profile(&merged) {
          log::warn!("Warning: Failed to clear profile PID: {e}");
        }
        detected_stop = true;
      }

      if detected_stop {
        if let Some(updated) = crate::auto_updater::AutoUpdater::instance()
          .update_profile_to_latest_installed(&app_handle, &merged)
        {
          merged = updated;
        }
      }

      // Emit profile update event to frontend
      if let Err(e) = events::emit("profile-updated", &merged) {
        log::warn!("Warning: Failed to emit profile update event: {e}");
      }
    }

    Ok(is_running)
  }

  // Check browser status using ChromiumManager
  async fn check_chromium_status(
    &self,
    app_handle: &tauri::AppHandle,
    profile: &BrowserProfile,
  ) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    let manager = self.chromium_manager;
    let profiles_dir = self.get_profiles_dir();
    let profile_data_path =
      crate::ephemeral_dirs::get_effective_profile_path(profile, &profiles_dir);
    let profile_path_str = profile_data_path.to_string_lossy();

    // Check if there's a running browser instance for this profile
    match manager.find_chromium_by_profile(&profile_path_str).await {
      Some(chromium_process) => {
        // Found a running instance, update profile with process info if changed
        let profiles_dir = self.get_profiles_dir();
        let profile_uuid_dir = profiles_dir.join(profile.id.to_string());
        let metadata_file = profile_uuid_dir.join("metadata.json");
        let metadata_exists = metadata_file.exists();

        if metadata_exists {
          // Load latest to avoid overwriting other fields
          let mut latest: BrowserProfile = match std::fs::read_to_string(&metadata_file)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
          {
            Some(p) => p,
            None => profile.clone(),
          };

          if latest.process_id != chromium_process.processId {
            let old_pid = latest.process_id;
            latest.process_id = chromium_process.processId;
            if let Err(e) = self.save_profile(&latest) {
              log::warn!("Warning: Failed to update profile with process info: {e}");
            }
            if let (Some(prev), Some(new)) = (old_pid, chromium_process.processId) {
              let _ = crate::proxy_manager::PROXY_MANAGER.update_proxy_pid(prev, new);
            }

            // Emit profile update event to frontend
            if let Err(e) = events::emit("profile-updated", &latest) {
              log::warn!("Warning: Failed to emit profile update event: {e}");
            }

            log::info!(
              "Browser process has started for profile '{}' with PID: {:?}",
              profile.name,
              chromium_process.processId
            );
          }
        }
        Ok(true)
      }
      None => {
        // No running instance found, clear process ID if set
        if profile.ephemeral {
          crate::ephemeral_dirs::remove_ephemeral_dir(&profile.id.to_string());
        }

        let profiles_dir = self.get_profiles_dir();
        let profile_uuid_dir = profiles_dir.join(profile.id.to_string());
        let metadata_file = profile_uuid_dir.join("metadata.json");
        let metadata_exists = metadata_file.exists();

        if metadata_exists {
          let mut latest: BrowserProfile = match std::fs::read_to_string(&metadata_file)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
          {
            Some(p) => p,
            None => profile.clone(),
          };

          if latest.process_id.is_some() {
            latest.process_id = None;
            if let Err(e) = self.save_profile(&latest) {
              log::warn!("Warning: Failed to clear profile process info: {e}");
            }

            if let Some(updated) = crate::auto_updater::AutoUpdater::instance()
              .update_profile_to_latest_installed(app_handle, &latest)
            {
              latest = updated;
            }

            if let Err(e) = events::emit("profile-updated", &latest) {
              log::warn!("Warning: Failed to emit profile update event: {e}");
            }
          }
        }
        Ok(false)
      }
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  use tempfile::TempDir;

  fn create_test_profile_manager() -> (&'static ProfileManager, TempDir) {
    let temp_dir = TempDir::new().unwrap();

    // Mock the base directories by setting environment variables
    unsafe {
      std::env::set_var("HOME", temp_dir.path());
    }

    let profile_manager = ProfileManager::instance();
    (profile_manager, temp_dir)
  }

  #[test]
  fn test_profile_manager_creation() {
    let (_manager, _temp_dir) = create_test_profile_manager();
    // If we get here without panicking, the test passes
  }

  #[test]
  fn test_get_profiles_dir() {
    let (manager, _temp_dir) = create_test_profile_manager();
    let profiles_dir = manager.get_profiles_dir();

    assert!(
      profiles_dir.to_string_lossy().contains("DucklingBrowser"),
      "Profiles dir should contain DucklingBrowser"
    );
    assert!(
      profiles_dir.to_string_lossy().contains("profiles"),
      "Profiles dir should contain profiles"
    );
  }

  #[test]
  fn test_get_binaries_dir() {
    let (manager, _temp_dir) = create_test_profile_manager();

    let binaries_dir = manager.get_binaries_dir();
    let path_str = binaries_dir.to_string_lossy();

    assert!(
      path_str.contains("DucklingBrowser"),
      "Binaries dir should contain DucklingBrowser"
    );
    assert!(
      path_str.contains("binaries"),
      "Binaries dir should contain binaries"
    );
  }

  #[test]
  fn test_normalize_launch_hook_accepts_http_and_https() {
    let http =
      ProfileManager::normalize_launch_hook(Some(" http://localhost:3000/hook ".to_string()))
        .unwrap();
    let https = ProfileManager::normalize_launch_hook(Some(
      "https://example.com/hooks/profile-launch".to_string(),
    ))
    .unwrap();

    assert_eq!(http.as_deref(), Some("http://localhost:3000/hook"));
    assert_eq!(
      https.as_deref(),
      Some("https://example.com/hooks/profile-launch")
    );
  }

  #[test]
  fn test_normalize_launch_hook_clears_empty_values() {
    let result = ProfileManager::normalize_launch_hook(Some("   ".to_string())).unwrap();
    assert!(result.is_none());
  }

  #[test]
  fn test_normalize_launch_hook_rejects_invalid_scheme() {
    let err = ProfileManager::normalize_launch_hook(Some("ftp://example.com/hook".to_string()))
      .unwrap_err();
    assert!(err.to_string().contains("http or https"));
  }

  #[test]
  fn test_validate_launch_hook_accepts_https_url() {
    let result = super::validate_launch_hook(Some("https://example.com/track")).unwrap();
    assert_eq!(result.as_deref(), Some("https://example.com/track"));
  }

  #[test]
  fn test_validate_launch_hook_rejects_garbage_with_code() {
    let err = super::validate_launch_hook(Some("not a url")).unwrap_err();
    let parsed: serde_json::Value = serde_json::from_str(&err).expect("error must be JSON");
    assert_eq!(parsed["code"], "INVALID_LAUNCH_HOOK_URL");
  }

  #[test]
  fn test_validate_launch_hook_rejects_non_http_scheme_with_code() {
    let err = super::validate_launch_hook(Some("ftp://example.com/hook")).unwrap_err();
    let parsed: serde_json::Value = serde_json::from_str(&err).expect("error must be JSON");
    assert_eq!(parsed["code"], "INVALID_LAUNCH_HOOK_URL");
  }

  #[test]
  fn test_validate_launch_hook_empty_clears_hook() {
    let result = super::validate_launch_hook(Some("")).unwrap();
    assert!(result.is_none());

    let result_ws = super::validate_launch_hook(Some("   ")).unwrap();
    assert!(result_ws.is_none());

    let result_none = super::validate_launch_hook(None).unwrap();
    assert!(result_none.is_none());
  }

  #[test]
  fn test_resolve_batch_names_explicit_list() {
    let names = super::ProfileManager::resolve_batch_names(
      &["Alpha".to_string(), "  Beta  ".to_string(), "".to_string()],
      None,
      None,
    )
    .unwrap();
    assert_eq!(names, vec!["Alpha", "Beta"]);
  }

  #[test]
  fn test_resolve_batch_names_prefix_and_count() {
    let names = super::ProfileManager::resolve_batch_names(&[], Some("Account"), Some(3)).unwrap();
    assert_eq!(names, vec!["Account 1", "Account 2", "Account 3"]);
  }

  #[test]
  fn test_resolve_batch_names_rejects_empty_prefix() {
    let err = super::ProfileManager::resolve_batch_names(&[], Some("  "), Some(3)).unwrap_err();
    let parsed: serde_json::Value = serde_json::from_str(&err).unwrap();
    assert_eq!(parsed["code"], "BATCH_CREATE_EMPTY_PREFIX");
  }

  #[test]
  fn test_resolve_batch_names_rejects_no_names() {
    let err = super::ProfileManager::resolve_batch_names(&[], None, None).unwrap_err();
    let parsed: serde_json::Value = serde_json::from_str(&err).unwrap();
    assert_eq!(parsed["code"], "BATCH_CREATE_NO_NAMES");
  }

  #[test]
  fn test_resolve_batch_names_rejects_over_cap() {
    let names: Vec<String> = (0..(super::ProfileManager::MAX_BATCH_CREATE_PROFILES + 1))
      .map(|i| format!("P{i}"))
      .collect();
    let err = super::ProfileManager::resolve_batch_names(&names, None, None).unwrap_err();
    let parsed: serde_json::Value = serde_json::from_str(&err).unwrap();
    assert_eq!(parsed["code"], "BATCH_CREATE_TOO_MANY");
    assert_eq!(
      parsed["params"]["requested"],
      super::ProfileManager::MAX_BATCH_CREATE_PROFILES + 1
    );
  }

  #[test]
  fn test_create_batch_many_creates_profiles_with_shared_fingerprint() {
    let temp_dir = TempDir::new().unwrap();
    let _guard = crate::app_dirs::set_test_data_dir(temp_dir.path().to_path_buf());
    let manager = ProfileManager::instance();
    let fingerprint = r#"{"timezone":"UTC","language":"en-US"}"#.to_string();
    let req = super::BatchCreateProfilesRequest {
      names: vec!["Batch One".to_string(), "Batch Two".to_string()],
      browser: "chromium".to_string(),
      version: "150.0.7871.100".to_string(),
      release_type: "stable".to_string(),
      chromium_config: Some(crate::chromium_manager::ChromiumConfig {
        fingerprint: Some(fingerprint.clone()),
        ..Default::default()
      }),
      ..Default::default()
    };

    let results = manager.create_batch_many(
      vec!["Batch One".to_string(), "Batch Two".to_string()],
      &req,
      req.chromium_config.clone(),
      None,
      &mut std::collections::HashSet::new(),
    );

    assert_eq!(results.len(), 2);
    assert!(results.iter().all(|r| r.ok), "{:?}", results);
    let profiles_dir = manager.get_profiles_dir();
    for result in &results {
      let profile = result.profile.as_ref().unwrap();
      assert_eq!(
        profile
          .chromium_config
          .as_ref()
          .unwrap()
          .fingerprint
          .as_deref(),
        Some(fingerprint.as_str())
      );
      assert_eq!(profile.browser, "chromium");
      assert!(
        profiles_dir
          .join(profile.id.to_string())
          .join("metadata.json")
          .exists(),
        "metadata.json must exist for {}",
        profile.name
      );
    }

    let listed = manager.list_profiles().unwrap();
    assert_eq!(listed.len(), 2);
  }

  #[test]
  fn test_create_batch_many_reports_duplicate_names_per_item() {
    let temp_dir = TempDir::new().unwrap();
    let _guard = crate::app_dirs::set_test_data_dir(temp_dir.path().to_path_buf());
    let manager = ProfileManager::instance();
    let req = super::BatchCreateProfilesRequest {
      names: vec!["Taken".to_string()],
      browser: "chromium".to_string(),
      version: "150.0.7871.100".to_string(),
      release_type: "stable".to_string(),
      chromium_config: Some(crate::chromium_manager::ChromiumConfig {
        fingerprint: Some(r#"{"timezone":"UTC"}"#.to_string()),
        ..Default::default()
      }),
      ..Default::default()
    };

    let mut existing = std::collections::HashSet::new();
    existing.insert("taken".to_string());
    let results = manager.create_batch_many(
      vec!["Taken".to_string(), "Fresh".to_string()],
      &req,
      req.chromium_config.clone(),
      None,
      &mut existing,
    );

    assert_eq!(results.len(), 2);
    assert!(!results[0].ok);
    assert!(results[0].error.is_some());
    assert!(results[1].ok);
  }
}

#[allow(clippy::too_many_arguments)]
#[tauri::command]
pub async fn create_browser_profile_with_group(
  app_handle: tauri::AppHandle,
  name: String,
  browser: String,
  version: String,
  release_type: String,
  proxy_id: Option<String>,
  vpn_id: Option<String>,
  chromium_config: Option<ChromiumConfig>,
  group_id: Option<String>,
  ephemeral: bool,
  dns_blocklist: Option<String>,
  launch_hook: Option<String>,
) -> Result<BrowserProfile, String> {
  let profile_manager = ProfileManager::instance();
  profile_manager
    .create_profile_with_group(
      &app_handle,
      &name,
      &browser,
      &version,
      &release_type,
      proxy_id,
      vpn_id,
      chromium_config,
      group_id,
      ephemeral,
      dns_blocklist,
      launch_hook,
    )
    .await
    .map_err(|e| crate::wrap_backend_error(e, "Failed to create profile"))
}

#[tauri::command]
pub fn list_browser_profiles() -> Result<Vec<BrowserProfile>, String> {
  let profile_manager = ProfileManager::instance();
  profile_manager
    .list_profiles()
    .map_err(|e| format!("Failed to list profiles: {e}"))
}

#[tauri::command]
pub async fn update_profile_proxy(
  app_handle: tauri::AppHandle,
  profile_id: String,
  proxy_id: Option<String>,
) -> Result<BrowserProfile, String> {
  let profile_manager = ProfileManager::instance();
  profile_manager
    .update_profile_proxy(app_handle, &profile_id, proxy_id)
    .await
    .map_err(|e| format!("Failed to update profile: {e}"))
}

#[tauri::command]
pub async fn update_profile_vpn(
  app_handle: tauri::AppHandle,
  profile_id: String,
  vpn_id: Option<String>,
) -> Result<BrowserProfile, String> {
  let profile_manager = ProfileManager::instance();
  profile_manager
    .update_profile_vpn(app_handle, &profile_id, vpn_id)
    .await
    .map_err(|e| format!("Failed to update profile VPN: {e}"))
}

#[tauri::command]
pub fn update_profile_tags(
  app_handle: tauri::AppHandle,
  profile_id: String,
  tags: Vec<String>,
) -> Result<BrowserProfile, String> {
  let profile_manager = ProfileManager::instance();
  profile_manager
    .update_profile_tags(&app_handle, &profile_id, tags)
    .map_err(|e| format!("Failed to update profile tags: {e}"))
}

#[tauri::command]
pub fn update_profile_note(
  app_handle: tauri::AppHandle,
  profile_id: String,
  note: Option<String>,
) -> Result<BrowserProfile, String> {
  let profile_manager = ProfileManager::instance();
  profile_manager
    .update_profile_note(&app_handle, &profile_id, note)
    .map_err(|e| format!("Failed to update profile note: {e}"))
}

#[tauri::command]
pub fn update_profile_window_color(
  app_handle: tauri::AppHandle,
  profile_id: String,
  window_color: Option<String>,
) -> Result<BrowserProfile, String> {
  ProfileManager::instance()
    .update_profile_window_color(&app_handle, &profile_id, window_color)
    .map_err(|e| format!("Failed to update profile window color: {e}"))
}

#[tauri::command]
pub fn update_profile_clear_on_close(
  app_handle: tauri::AppHandle,
  profile_id: String,
  clear_on_close: bool,
) -> Result<BrowserProfile, String> {
  ProfileManager::instance()
    .update_profile_clear_on_close(&app_handle, &profile_id, clear_on_close)
    .map_err(crate::profile_importer::error_to_code_string)
}

#[tauri::command]
pub fn update_profile_download_dir(
  app_handle: tauri::AppHandle,
  profile_id: String,
  download_dir: Option<String>,
) -> Result<BrowserProfile, String> {
  ProfileManager::instance()
    .update_profile_download_dir(&app_handle, &profile_id, download_dir)
    .map_err(|e| format!("Failed to update profile download folder: {e}"))
}

#[tauri::command]
pub fn update_profile_allow_agent_downloads(
  app_handle: tauri::AppHandle,
  profile_id: String,
  allow: bool,
) -> Result<BrowserProfile, String> {
  ProfileManager::instance()
    .update_profile_allow_agent_downloads(&app_handle, &profile_id, allow)
    .map_err(|e| format!("Failed to update profile agent downloads: {e}"))
}

#[tauri::command]
pub fn update_profile_agent_auto_approve(
  app_handle: tauri::AppHandle,
  profile_id: String,
  auto_approve: bool,
) -> Result<BrowserProfile, String> {
  ProfileManager::instance()
    .update_profile_agent_auto_approve(&app_handle, &profile_id, auto_approve)
    .map_err(|e| format!("Failed to update profile automation: {e}"))
}

#[tauri::command]
pub fn update_profile_agent_pair(
  app_handle: tauri::AppHandle,
  profile_id: String,
  key_id: Option<String>,
  agent_id: Option<String>,
) -> Result<BrowserProfile, String> {
  ProfileManager::instance()
    .update_profile_agent_pair(&app_handle, &profile_id, key_id, agent_id)
    .map_err(|e| format!("Failed to update profile agent pair: {e}"))
}

/// Validate a launch hook value. Returns `Ok(None)` for "clear the hook"
/// (`None`, empty, or whitespace-only), `Ok(Some(_))` for a valid http(s)
/// URL, or `Err` with the `INVALID_LAUNCH_HOOK_URL` code payload.
pub(crate) fn validate_launch_hook(launch_hook: Option<&str>) -> Result<Option<String>, String> {
  let Some(raw) = launch_hook else {
    return Ok(None);
  };
  let trimmed = raw.trim();
  if trimmed.is_empty() {
    return Ok(None);
  }
  let ok = url::Url::parse(trimmed)
    .ok()
    .map(|u| matches!(u.scheme(), "http" | "https"))
    .unwrap_or(false);
  if !ok {
    return Err(serde_json::json!({ "code": "INVALID_LAUNCH_HOOK_URL" }).to_string());
  }
  Ok(Some(trimmed.to_string()))
}

#[tauri::command]
pub fn update_profile_launch_hook(
  app_handle: tauri::AppHandle,
  profile_id: String,
  launch_hook: Option<String>,
) -> Result<BrowserProfile, String> {
  validate_launch_hook(launch_hook.as_deref())?;
  let profile_manager = ProfileManager::instance();
  profile_manager
    .update_profile_launch_hook(&app_handle, &profile_id, launch_hook)
    .map_err(|e| format!("Failed to update profile launch hook: {e}"))
}

#[tauri::command]
pub fn update_profile_proxy_bypass_rules(
  app_handle: tauri::AppHandle,
  profile_id: String,
  rules: Vec<String>,
) -> Result<BrowserProfile, String> {
  let profile_manager = ProfileManager::instance();
  profile_manager
    .update_profile_proxy_bypass_rules(&app_handle, &profile_id, rules)
    .map_err(|e| format!("Failed to update proxy bypass rules: {e}"))
}

#[tauri::command]
pub fn update_profile_dns_blocklist(
  profile_id: String,
  dns_blocklist: Option<String>,
) -> Result<BrowserProfile, String> {
  let profile_manager = ProfileManager::instance();
  profile_manager
    .update_profile_dns_blocklist(&profile_id, dns_blocklist)
    .map_err(|e| format!("Failed to update DNS blocklist: {e}"))
}

#[tauri::command]
pub async fn check_browser_status(
  app_handle: tauri::AppHandle,
  profile: BrowserProfile,
) -> Result<bool, String> {
  let profile_manager = ProfileManager::instance();
  profile_manager
    .check_browser_status(app_handle, &profile)
    .await
    .map_err(|e| format!("Failed to check browser status: {e}"))
}

#[tauri::command]
pub fn rename_profile(
  app_handle: tauri::AppHandle,
  profile_id: String,
  new_name: String,
) -> Result<BrowserProfile, String> {
  let profile_manager = ProfileManager::instance();
  profile_manager
    .rename_profile(&app_handle, &profile_id, &new_name)
    .map_err(|e| crate::wrap_backend_error(e, "Failed to rename profile"))
}

#[allow(clippy::too_many_arguments)]
#[tauri::command]
pub async fn create_browser_profile_new(
  app_handle: tauri::AppHandle,
  name: String,
  browser_str: String,
  version: String,
  release_type: String,
  proxy_id: Option<String>,
  vpn_id: Option<String>,
  chromium_config: Option<ChromiumConfig>,
  group_id: Option<String>,
  ephemeral: Option<bool>,
  dns_blocklist: Option<String>,
  launch_hook: Option<String>,
) -> Result<BrowserProfile, String> {
  let fingerprint_os = chromium_config.as_ref().and_then(|c| c.os.as_deref());

  if !crate::cloud_auth::CLOUD_AUTH
    .is_fingerprint_os_allowed(fingerprint_os)
    .await
  {
    return Err("Fingerprint OS spoofing requires an active Pro subscription".to_string());
  }

  // A dead/unreachable proxy or VPN (or a 402 from an expired proxy
  // subscription) cancels creation with a translatable error.
  crate::validate_profile_network(proxy_id.as_deref(), vpn_id.as_deref()).await?;

  let browser_type =
    BrowserType::from_str(&browser_str).map_err(|e| format!("Invalid browser type: {e}"))?;
  create_browser_profile_with_group(
    app_handle,
    name,
    browser_type.as_str().to_string(),
    version,
    release_type,
    proxy_id,
    vpn_id,
    chromium_config,
    group_id,
    ephemeral.unwrap_or(false),
    dns_blocklist,
    launch_hook,
  )
  .await
}

#[tauri::command]
pub async fn batch_create_browser_profiles(
  app_handle: tauri::AppHandle,
  request: BatchCreateProfilesRequest,
) -> Result<Vec<BatchCreateResult>, String> {
  // A dead/unreachable proxy or VPN cancels the batch before any profile is
  // created (mirrors create_browser_profile_new; validated once, not per name).
  crate::validate_profile_network(request.proxy_id.as_deref(), request.vpn_id.as_deref()).await?;

  ProfileManager::instance()
    .batch_create_profiles(&app_handle, &request)
    .await
    .map_err(|e| crate::wrap_backend_error(e, "Failed to batch create profiles"))
}

#[tauri::command]
pub async fn update_chromium_config(
  app_handle: tauri::AppHandle,
  profile_id: String,
  config: ChromiumConfig,
) -> Result<(), String> {
  if config.fingerprint.is_some()
    && !crate::cloud_auth::CLOUD_AUTH
      .can_use_cross_os_fingerprints()
      .await
  {
    return Err(serde_json::json!({ "code": "FINGERPRINT_REQUIRES_PRO" }).to_string());
  }

  if !crate::cloud_auth::CLOUD_AUTH
    .is_fingerprint_os_allowed(config.os.as_deref())
    .await
  {
    return Err("Fingerprint OS spoofing requires an active Pro subscription".to_string());
  }

  let profile_manager = ProfileManager::instance();
  profile_manager
    .update_chromium_config(app_handle, &profile_id, config)
    .await
    .map_err(|e| format!("Failed to update browser config: {e}"))
}

#[tauri::command]
pub fn clone_profile(profile_id: String, name: Option<String>) -> Result<BrowserProfile, String> {
  ProfileManager::instance()
    .clone_profile(&profile_id, name)
    .map_err(|e| format!("Failed to clone profile: {e}"))
}

#[tauri::command]
pub fn delete_profile(app_handle: tauri::AppHandle, profile_id: String) -> Result<(), String> {
  ProfileManager::instance()
    .delete_profile(&app_handle, &profile_id)
    .map_err(|e| format!("Failed to delete profile: {e}"))
}

lazy_static::lazy_static! {
  static ref PROFILE_MANAGER: ProfileManager = ProfileManager::new();
}

#[cfg(test)]
mod index_tests {
  use super::*;

  fn make_profile(name: &str) -> BrowserProfile {
    BrowserProfile {
      id: uuid::Uuid::new_v4(),
      name: name.to_string(),
      browser: "chromium".to_string(),
      version: "stable".to_string(),
      ..BrowserProfile::default()
    }
  }

  fn saved_profile(manager: &ProfileManager, name: &str) -> BrowserProfile {
    let profile = make_profile(name);
    manager.save_profile(&profile).unwrap();
    profile
  }

  #[test]
  fn list_serves_fresh_snapshot_and_self_heals_external_edits() {
    let tmp = tempfile::tempdir().unwrap();
    let _guard = crate::app_dirs::set_test_data_dir(tmp.path().to_path_buf());
    let manager = ProfileManager::instance();

    let first = saved_profile(manager, "alpha");
    let second = saved_profile(manager, "beta");

    let names = |profiles: &[BrowserProfile]| {
      let mut names: Vec<_> = profiles.iter().map(|p| p.name.clone()).collect();
      names.sort();
      names
    };
    let listed = manager.list_profiles().unwrap();
    assert_eq!(names(&listed), vec!["alpha", "beta"]);

    // Editing metadata.json directly (bypassing save_profile) is picked up
    // without a directory-level rescan: file mtime changed, dir mtime did not.
    let metadata_file = tmp
      .path()
      .join("profiles")
      .join(first.id.to_string())
      .join("metadata.json");
    let mut external = first.clone();
    external.name = "alpha-renamed-externally".to_string();
    atomic_write(
      &metadata_file,
      serde_json::to_string_pretty(&external).unwrap().as_bytes(),
    )
    .unwrap();

    let listed = manager.list_profiles().unwrap();
    assert_eq!(names(&listed), vec!["alpha-renamed-externally", "beta"]);

    // Deleting a metadata.json externally also self-heals: the entry vanishes.
    fs::remove_file(&metadata_file).unwrap();
    let listed = manager.list_profiles().unwrap();
    assert_eq!(names(&listed), vec!["beta"]);

    // And a fresh profile via the normal path reappears through the upsert.
    let third = saved_profile(manager, "gamma");
    let listed = manager.list_profiles().unwrap();
    assert_eq!(names(&listed), vec!["beta", "gamma"]);
    let _ = third;
    let _ = second;
  }

  #[test]
  fn save_keeps_the_index_fresh_and_get_by_id_uses_it() {
    let tmp = tempfile::tempdir().unwrap();
    let _guard = crate::app_dirs::set_test_data_dir(tmp.path().to_path_buf());
    let manager = ProfileManager::instance();

    let mut profile = saved_profile(manager, "alpha");

    // Fast path: served straight from the index.
    let cached = manager.get_profile_by_id(&profile.id).unwrap().unwrap();
    assert_eq!(cached.name, "alpha");
    assert_eq!(cached.id, profile.id);

    // Unknown id returns None without a scan.
    assert!(manager
      .get_profile_by_id(&uuid::Uuid::new_v4())
      .unwrap()
      .is_none());

    // A save refreshes the entry so the next reads see the new name.
    profile.name = "alpha-v2".to_string();
    manager.save_profile(&profile).unwrap();
    let cached = manager.get_profile_by_id(&profile.id).unwrap().unwrap();
    assert_eq!(cached.name, "alpha-v2");
    assert_eq!(
      manager
        .list_profiles()
        .unwrap()
        .iter()
        .find(|p| p.id == profile.id)
        .unwrap()
        .name,
      "alpha-v2"
    );
  }

  #[test]
  fn cache_resets_when_the_store_directory_changes() {
    let tmp = tempfile::tempdir().unwrap();
    let _guard = crate::app_dirs::set_test_data_dir(tmp.path().to_path_buf());
    let manager = ProfileManager::instance();

    let first = saved_profile(manager, "alpha");
    assert_eq!(manager.list_profiles().unwrap().len(), 1);

    // A different data dir must not see the cached entries from the old one.
    let other = tempfile::tempdir().unwrap();
    let _guard = crate::app_dirs::set_test_data_dir(other.path().to_path_buf());
    let second = saved_profile(manager, "beta");
    let listed = manager.list_profiles().unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, second.id);
    assert!(manager.get_profile_by_id(&first.id).unwrap().is_none());
  }
}
