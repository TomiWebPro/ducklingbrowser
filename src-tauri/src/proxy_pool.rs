use crate::events;
use crate::profile::manager::ProfileManager;
use crate::profile::types::BrowserProfile;
use crate::proxy_manager::{now_secs, CLOUD_PROXY_ID, PROXY_MANAGER};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Mutex;

/// A named set of stored proxies. Membership lives entirely in `proxy_ids`
/// (the `StoredProxy` schema is untouched). Pools are local-only for now;
/// sync integration is a later phase.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyPool {
  pub id: String,
  pub name: String,
  pub proxy_ids: Vec<String>,
  #[serde(default)]
  pub updated_at: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PoolAssignResult {
  pub profile_id: String,
  pub ok: bool,
  pub proxy_id: Option<String>,
  pub error: Option<String>,
}

pub struct ProxyPoolManager {
  pools: Mutex<HashMap<String, ProxyPool>>,
  round_robin: Mutex<HashMap<String, usize>>,
}

lazy_static::lazy_static! {
    pub static ref PROXY_POOL_MANAGER: ProxyPoolManager = ProxyPoolManager::new();
}

impl ProxyPoolManager {
  fn new() -> Self {
    let manager = ProxyPoolManager {
      pools: Mutex::new(HashMap::new()),
      round_robin: Mutex::new(HashMap::new()),
    };
    manager.reload();
    manager
  }

  #[cfg(test)]
  pub fn reset_for_tests(&self) {
    self.pools.lock().unwrap().clear();
    self.round_robin.lock().unwrap().clear();
    self.reload();
  }

  fn pools_path(&self) -> PathBuf {
    crate::app_dirs::settings_dir().join("proxy_pools.json")
  }

  fn reload(&self) {
    let path = self.pools_path();
    if !path.exists() {
      return;
    }
    match std::fs::read_to_string(&path) {
      Ok(content) => match serde_json::from_str::<Vec<ProxyPool>>(&content) {
        Ok(pools) => {
          let mut map = self.pools.lock().unwrap();
          map.clear();
          for pool in pools {
            map.insert(pool.id.clone(), pool);
          }
        }
        Err(e) => log::warn!("Failed to parse proxy pools file: {e}"),
      },
      Err(e) => log::warn!("Failed to read proxy pools file: {e}"),
    }
  }

  fn persist(&self) {
    let pools = self.pools.lock().unwrap();
    let mut sorted: Vec<&ProxyPool> = pools.values().collect();
    sorted.sort_by(|a, b| a.name.cmp(&b.name));
    let path = self.pools_path();
    if let Some(dir) = path.parent() {
      let _ = std::fs::create_dir_all(dir);
    }
    let Ok(content) = serde_json::to_string_pretty(&sorted) else {
      log::warn!("Failed to serialize proxy pools");
      return;
    };
    let tmp = path.with_extension("json.tmp");
    if let Err(e) = std::fs::write(&tmp, content) {
      log::warn!("Failed to write proxy pools file: {e}");
      return;
    }
    if let Err(e) = std::fs::rename(&tmp, &path) {
      log::warn!("Failed to write proxy pools file: {e}");
    }
  }

  fn validate_proxy_ids(&self, proxy_ids: &[String]) -> Result<(), String> {
    for id in proxy_ids {
      if *id == CLOUD_PROXY_ID || PROXY_MANAGER.get_proxy_settings_by_id(id).is_none() {
        return Err(
          serde_json::json!({ "code": "POOL_INVALID_PROXY", "params": { "id": id } }).to_string(),
        );
      }
    }
    Ok(())
  }

  /// Members that still exist as stored proxies (skips cloud proxy and deleted proxies).
  fn live_members(&self, pool: &ProxyPool) -> Vec<String> {
    pool
      .proxy_ids
      .iter()
      .filter(|id| **id != CLOUD_PROXY_ID && PROXY_MANAGER.get_proxy_settings_by_id(id).is_some())
      .cloned()
      .collect()
  }

  pub fn create_pool(&self, name: String, proxy_ids: Vec<String>) -> Result<ProxyPool, String> {
    if name.trim().is_empty() {
      return Err(serde_json::json!({ "code": "POOL_EMPTY_NAME" }).to_string());
    }
    {
      let pools = self.pools.lock().unwrap();
      if pools
        .values()
        .any(|p| p.name.eq_ignore_ascii_case(name.trim()))
      {
        return Err(serde_json::json!({ "code": "POOL_DUPLICATE_NAME" }).to_string());
      }
    }
    let deduped = Self::dedupe(proxy_ids);
    if deduped.is_empty() {
      return Err(serde_json::json!({ "code": "POOL_NO_MEMBERS" }).to_string());
    }
    self.validate_proxy_ids(&deduped)?;

    let pool = ProxyPool {
      id: uuid::Uuid::new_v4().to_string(),
      name: name.trim().to_string(),
      proxy_ids: deduped,
      updated_at: Some(now_secs()),
    };
    self
      .pools
      .lock()
      .unwrap()
      .insert(pool.id.clone(), pool.clone());
    self.persist();
    let _ = events::emit_empty("proxy-pools-changed");
    Ok(pool)
  }

  pub fn list_pools(&self) -> Vec<ProxyPool> {
    let pools = self.pools.lock().unwrap();
    let mut sorted: Vec<ProxyPool> = pools.values().cloned().collect();
    sorted.sort_by(|a, b| a.name.cmp(&b.name));
    sorted
  }

  pub fn get_pool(&self, pool_id: &str) -> Option<ProxyPool> {
    self.pools.lock().unwrap().get(pool_id).cloned()
  }

  pub fn update_pool(
    &self,
    pool_id: String,
    name: String,
    proxy_ids: Vec<String>,
  ) -> Result<ProxyPool, String> {
    if name.trim().is_empty() {
      return Err(serde_json::json!({ "code": "POOL_EMPTY_NAME" }).to_string());
    }
    {
      let pools = self.pools.lock().unwrap();
      if !pools.contains_key(&pool_id) {
        return Err(
          serde_json::json!({ "code": "POOL_NOT_FOUND", "params": { "id": pool_id } }).to_string(),
        );
      }
      if pools
        .values()
        .any(|p| p.id != pool_id && p.name.eq_ignore_ascii_case(name.trim()))
      {
        return Err(serde_json::json!({ "code": "POOL_DUPLICATE_NAME" }).to_string());
      }
    }
    let deduped = Self::dedupe(proxy_ids);
    if deduped.is_empty() {
      return Err(serde_json::json!({ "code": "POOL_NO_MEMBERS" }).to_string());
    }
    self.validate_proxy_ids(&deduped)?;

    let mut pools = self.pools.lock().unwrap();
    let pool = pools.get_mut(&pool_id).unwrap();
    pool.name = name.trim().to_string();
    pool.proxy_ids = deduped;
    pool.updated_at = Some(now_secs());
    let updated = pool.clone();
    drop(pools);
    self.persist();
    let _ = events::emit_empty("proxy-pools-changed");
    Ok(updated)
  }

  pub fn delete_pool(&self, pool_id: &str) -> Result<(), String> {
    let removed = self.pools.lock().unwrap().remove(pool_id).is_some();
    if !removed {
      return Err(
        serde_json::json!({ "code": "POOL_NOT_FOUND", "params": { "id": pool_id } }).to_string(),
      );
    }
    self.round_robin.lock().unwrap().remove(pool_id);
    self.persist();
    let _ = events::emit_empty("proxy-pools-changed");
    Ok(())
  }

  fn dedupe(proxy_ids: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut deduped = Vec::new();
    for id in proxy_ids {
      if seen.insert(id.clone()) {
        deduped.push(id);
      }
    }
    deduped
  }

  pub fn pool_for_proxy(&self, proxy_id: &str) -> Option<ProxyPool> {
    let pools = self.pools.lock().unwrap();
    pools
      .values()
      .find(|p| p.proxy_ids.iter().any(|id| id == proxy_id))
      .cloned()
  }

  /// Round-robin pick of a live pool member (for initial distribution).
  pub fn next_member(&self, pool_id: &str) -> Result<String, String> {
    let pool = self.get_pool(pool_id).ok_or_else(|| {
      serde_json::json!({ "code": "POOL_NOT_FOUND", "params": { "id": pool_id } }).to_string()
    })?;
    let live = self.live_members(&pool);
    if live.is_empty() {
      return Err(serde_json::json!({ "code": "POOL_NO_MEMBERS" }).to_string());
    }
    let mut counters = self.round_robin.lock().unwrap();
    let counter = counters.entry(pool.id.clone()).or_insert(0);
    let idx = *counter % live.len();
    *counter = counter.wrapping_add(1);
    Ok(live[idx].clone())
  }

  /// Next pool member different from `current` (for rotation/failover).
  pub fn rotate_member(&self, pool_id: &str, current: Option<&str>) -> Result<String, String> {
    let pool = self.get_pool(pool_id).ok_or_else(|| {
      serde_json::json!({ "code": "POOL_NOT_FOUND", "params": { "id": pool_id } }).to_string()
    })?;
    let live = self.live_members(&pool);
    if live.len() < 2 {
      return Err(serde_json::json!({ "code": "POOL_SINGLE_MEMBER" }).to_string());
    }
    let mut counters = self.round_robin.lock().unwrap();
    let counter = counters.entry(pool.id.clone()).or_insert(0);
    let mut idx = *counter % live.len();
    *counter = counter.wrapping_add(1);
    if live[idx] == current.unwrap_or("") {
      idx = (idx + 1) % live.len();
    }
    Ok(live[idx].clone())
  }

  /// Given a profile's current proxy, resolve the next pool member to rotate to.
  pub fn resolve_rotate_target(&self, profile: &BrowserProfile) -> Result<String, String> {
    let Some(current) = profile.proxy_id.as_deref() else {
      return Err(serde_json::json!({ "code": "PROFILE_NOT_IN_POOL" }).to_string());
    };
    let pool = self
      .pool_for_proxy(current)
      .ok_or_else(|| serde_json::json!({ "code": "PROFILE_NOT_IN_POOL" }).to_string())?;
    self.rotate_member(&pool.id, Some(current))
  }

  /// Distribute the pool's proxies across profiles (round-robin).
  pub async fn assign_profiles_to_pool(
    &self,
    app_handle: &tauri::AppHandle,
    pool_id: String,
    profile_ids: Vec<String>,
  ) -> Result<Vec<PoolAssignResult>, String> {
    let pool = self.get_pool(&pool_id).ok_or_else(|| {
      serde_json::json!({ "code": "POOL_NOT_FOUND", "params": { "id": pool_id } }).to_string()
    })?;
    if self.live_members(&pool).is_empty() {
      return Err(serde_json::json!({ "code": "POOL_NO_MEMBERS" }).to_string());
    }
    let manager = ProfileManager::instance();
    let mut results = Vec::new();
    for profile_id in profile_ids {
      let proxy_id = match self.next_member(&pool_id) {
        Ok(id) => id,
        Err(e) => {
          results.push(PoolAssignResult {
            profile_id: profile_id.clone(),
            ok: false,
            proxy_id: None,
            error: Some(e),
          });
          continue;
        }
      };
      match manager
        .update_profile_proxy(app_handle.clone(), &profile_id, Some(proxy_id.clone()))
        .await
      {
        Ok(profile) => results.push(PoolAssignResult {
          profile_id,
          ok: true,
          proxy_id: profile.proxy_id,
          error: None,
        }),
        Err(e) => results.push(PoolAssignResult {
          profile_id,
          ok: false,
          proxy_id: None,
          error: Some(e.to_string()),
        }),
      }
    }
    Ok(results)
  }

  /// Rotate a single profile to the next pool member (launch-time rotation v1).
  pub async fn rotate_profile_proxy(
    &self,
    app_handle: &tauri::AppHandle,
    profile_id: &str,
  ) -> Result<ProxySettingsDto, String> {
    let profile = ProfileManager::instance()
      .list_profiles()
      .map_err(|e| format!("Failed to list profiles: {e}"))?
      .into_iter()
      .find(|p| p.id.to_string() == profile_id)
      .ok_or_else(|| {
        serde_json::json!({ "code": "PROFILE_NOT_FOUND", "params": { "id": profile_id } })
          .to_string()
      })?;
    let next = self.resolve_rotate_target(&profile)?;
    ProfileManager::instance()
      .update_profile_proxy(app_handle.clone(), profile_id, Some(next.clone()))
      .await
      .map_err(|e| format!("Failed to update profile proxy: {e}"))?;
    let settings = PROXY_MANAGER
      .get_proxy_settings_by_id(&next)
      .ok_or_else(|| {
        serde_json::json!({ "code": "POOL_INVALID_PROXY", "params": { "id": next } }).to_string()
      })?;
    Ok(ProxySettingsDto {
      id: next,
      proxy_settings: settings,
    })
  }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxySettingsDto {
  pub id: String,
  pub proxy_settings: crate::browser::ProxySettings,
}

type LaunchResult = Result<BrowserProfile, String>;

/// Launch a profile, retrying with the next pool member whenever the launch
/// fails and the profile's current proxy belongs to a pool with spare live
/// members. Stops when every member has been tried once.
pub async fn launch_with_pool_failover<L, U>(
  profile: BrowserProfile,
  launch: L,
  update_proxy: U,
) -> LaunchResult
where
  L: Fn(BrowserProfile) -> futures_util::future::BoxFuture<'static, LaunchResult>,
  U: Fn(&str, String) -> futures_util::future::BoxFuture<'static, Result<BrowserProfile, String>>,
{
  let manager = &*PROXY_POOL_MANAGER;
  let pool = profile
    .proxy_id
    .as_deref()
    .and_then(|pid| manager.pool_for_proxy(pid));
  let Some(pool) = pool else {
    return launch(profile).await;
  };
  if manager.live_members(&pool).len() < 2 {
    return launch(profile).await;
  }

  let mut tried: HashSet<String> = HashSet::new();
  let mut current = profile;
  let mut first_error = None;
  loop {
    if let Some(pid) = current.proxy_id.clone() {
      tried.insert(pid);
    }
    match launch(current.clone()).await {
      Ok(launched) => return Ok(launched),
      Err(e) => {
        log::info!(
          "Launch failed for pool member ({}), attempting failover: {e}",
          current.name
        );
        if first_error.is_none() {
          first_error = Some(e);
        }
        let candidates: Vec<String> = manager
          .live_members(&pool)
          .into_iter()
          .filter(|id| !tried.contains(id))
          .collect();
        let Some(next) = candidates.first().cloned() else {
          return Err(first_error.unwrap_or_else(|| "Pool failover exhausted".to_string()));
        };
        match update_proxy(&current.id.to_string(), next).await {
          Ok(updated) => current = updated,
          Err(update_err) => {
            log::warn!("Failed to update profile proxy during failover: {update_err}");
            return Err(first_error.unwrap_or(update_err));
          }
        }
      }
    }
  }
}

/// Convenience wrapper wiring real launch + persistence into failover retries.
pub async fn launch_browser_profile_with_pool_failover(
  app_handle: tauri::AppHandle,
  profile: BrowserProfile,
  url: Option<String>,
  remote_debugging_port: Option<u16>,
  headless: bool,
  force_new: bool,
) -> LaunchResult {
  launch_with_pool_failover(
    profile,
    |p| {
      let app_handle = app_handle.clone();
      let url = url.clone();
      Box::pin(crate::browser_runner::launch_browser_profile_impl(
        app_handle,
        p,
        url,
        remote_debugging_port,
        headless,
        force_new,
      ))
    },
    |profile_id, proxy_id| {
      let app_handle = app_handle.clone();
      let profile_id = profile_id.to_string();
      Box::pin(async move {
        ProfileManager::instance()
          .update_profile_proxy(app_handle, &profile_id, Some(proxy_id))
          .await
          .map_err(|e| e.to_string())
      })
    },
  )
  .await
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::browser::ProxySettings;
  use crate::proxy_manager::StoredProxy;
  use tempfile::TempDir;

  // The pool manager is a process-wide singleton and the test data dir is
  // global state; serialize pool tests so parallel tests don't clobber each
  // other's pools between reset_for_tests and assertion.
  static POOL_TESTS_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

  fn serial_guard() -> std::sync::MutexGuard<'static, ()> {
    POOL_TESTS_LOCK.lock().unwrap()
  }

  fn seed_proxy(name: &str) -> String {
    PROXY_MANAGER.insert_proxy_for_tests(StoredProxy::new(
      name.to_string(),
      ProxySettings {
        proxy_type: "http".to_string(),
        host: "127.0.0.1".to_string(),
        port: 8080,
        username: None,
        password: None,
      },
    ))
  }

  fn seed_pool(name: &str, proxy_names: &[&str]) -> (String, Vec<String>) {
    let ids: Vec<String> = proxy_names.iter().map(|n| seed_proxy(n)).collect();
    let pool = PROXY_POOL_MANAGER
      .create_pool(name.to_string(), ids.clone())
      .unwrap();
    (pool.id, ids)
  }

  fn profile_with_proxy(proxy_id: Option<String>) -> BrowserProfile {
    BrowserProfile {
      proxy_id,
      ..Default::default()
    }
  }

  #[test]
  fn test_pool_crud_roundtrip() {
    let temp_dir = TempDir::new().unwrap();
    let _serial = serial_guard();
    let _guard = crate::app_dirs::set_test_data_dir(temp_dir.path().to_path_buf());
    PROXY_POOL_MANAGER.reset_for_tests();

    let (pool_id, ids) = seed_pool("Roundtrip", &["r-a", "r-b", "r-c"]);
    assert_eq!(ids.len(), 3);
    let pool = PROXY_POOL_MANAGER.get_pool(&pool_id).unwrap();
    assert_eq!(pool.name, "Roundtrip");
    assert_eq!(pool.proxy_ids, ids);
    assert!(pool.updated_at.is_some());

    let updated = PROXY_POOL_MANAGER
      .update_pool(
        pool_id.clone(),
        "Roundtrip2".to_string(),
        vec![ids[1].clone()],
      )
      .unwrap();
    assert_eq!(updated.name, "Roundtrip2");
    assert_eq!(updated.proxy_ids, vec![ids[1].clone()]);

    assert_eq!(PROXY_POOL_MANAGER.list_pools().len(), 1);
    PROXY_POOL_MANAGER.delete_pool(&pool_id).unwrap();
    assert!(PROXY_POOL_MANAGER.list_pools().is_empty());
    assert!(PROXY_POOL_MANAGER.get_pool(&pool_id).is_none());
  }

  #[test]
  fn test_pool_persists_across_reload() {
    let temp_dir = TempDir::new().unwrap();
    let _serial = serial_guard();
    let _guard = crate::app_dirs::set_test_data_dir(temp_dir.path().to_path_buf());
    PROXY_POOL_MANAGER.reset_for_tests();

    let (pool_id, _) = seed_pool("Persist", &["p-a", "p-b"]);
    PROXY_POOL_MANAGER.reset_for_tests();
    let pool = PROXY_POOL_MANAGER.get_pool(&pool_id).unwrap();
    assert_eq!(pool.name, "Persist");
    assert_eq!(pool.proxy_ids.len(), 2);
  }

  #[test]
  fn test_legacy_pool_file_missing_updated_at_loads() {
    let temp_dir = TempDir::new().unwrap();
    let _serial = serial_guard();
    let _guard = crate::app_dirs::set_test_data_dir(temp_dir.path().to_path_buf());
    let path = crate::app_dirs::settings_dir().join("proxy_pools.json");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(
      &path,
      r#"[{"id":"legacy-1","name":"Legacy","proxy_ids":["p1","p2"]}]"#,
    )
    .unwrap();
    PROXY_POOL_MANAGER.reset_for_tests();
    let pool = PROXY_POOL_MANAGER.get_pool("legacy-1").unwrap();
    assert_eq!(pool.name, "Legacy");
    assert_eq!(pool.proxy_ids, vec!["p1".to_string(), "p2".to_string()]);
    assert!(pool.updated_at.is_none());
  }

  #[test]
  fn test_pool_validation() {
    let temp_dir = TempDir::new().unwrap();
    let _serial = serial_guard();
    let _guard = crate::app_dirs::set_test_data_dir(temp_dir.path().to_path_buf());
    PROXY_POOL_MANAGER.reset_for_tests();

    let err = PROXY_POOL_MANAGER
      .create_pool("Empty".to_string(), vec![])
      .unwrap_err();
    assert_eq!(
      serde_json::from_str::<serde_json::Value>(&err).unwrap()["code"],
      "POOL_NO_MEMBERS"
    );

    let err = PROXY_POOL_MANAGER
      .create_pool("Bad".to_string(), vec!["nonexistent".to_string()])
      .unwrap_err();
    let parsed: serde_json::Value = serde_json::from_str(&err).unwrap();
    assert_eq!(parsed["code"], "POOL_INVALID_PROXY");
    assert_eq!(parsed["params"]["id"], "nonexistent");

    let err = PROXY_POOL_MANAGER
      .create_pool("  ".to_string(), vec!["x".to_string()])
      .unwrap_err();
    assert_eq!(
      serde_json::from_str::<serde_json::Value>(&err).unwrap()["code"],
      "POOL_EMPTY_NAME"
    );
  }

  #[test]
  fn test_round_robin_distribution() {
    let temp_dir = TempDir::new().unwrap();
    let _serial = serial_guard();
    let _guard = crate::app_dirs::set_test_data_dir(temp_dir.path().to_path_buf());
    PROXY_POOL_MANAGER.reset_for_tests();

    let (pool_id, ids) = seed_pool("Robin", &["ro-a", "ro-b", "ro-c"]);
    let mut picks = Vec::new();
    for _ in 0..6 {
      picks.push(PROXY_POOL_MANAGER.next_member(&pool_id).unwrap());
    }
    let expected: Vec<String> = vec![ids[0].clone(), ids[1].clone(), ids[2].clone()]
      .into_iter()
      .cycle()
      .take(6)
      .collect();
    assert_eq!(picks, expected);
  }

  #[test]
  fn test_round_robin_skips_deleted_member() {
    let temp_dir = TempDir::new().unwrap();
    let _serial = serial_guard();
    let _guard = crate::app_dirs::set_test_data_dir(temp_dir.path().to_path_buf());
    PROXY_POOL_MANAGER.reset_for_tests();

    let (pool_id, ids) = seed_pool("Dead", &["dd-a", "dd-b"]);
    PROXY_MANAGER.delete_stored_proxy_for_tests(&ids[0]);
    for _ in 0..4 {
      assert_eq!(
        PROXY_POOL_MANAGER.next_member(&pool_id).unwrap(),
        ids[1],
        "deleted member must be skipped"
      );
    }
  }

  #[test]
  fn test_rotate_member_never_returns_current() {
    let temp_dir = TempDir::new().unwrap();
    let _serial = serial_guard();
    let _guard = crate::app_dirs::set_test_data_dir(temp_dir.path().to_path_buf());
    PROXY_POOL_MANAGER.reset_for_tests();

    let (pool_id, ids) = seed_pool("Rotate", &["rot-a", "rot-b", "rot-c"]);
    for _ in 0..10 {
      let pick = PROXY_POOL_MANAGER
        .rotate_member(&pool_id, Some(&ids[1]))
        .unwrap();
      assert_ne!(pick, ids[1]);
      assert!(ids.contains(&pick));
    }

    let (single_id, _) = seed_pool("Single", &["solo-a"]);
    let err = PROXY_POOL_MANAGER
      .rotate_member(&single_id, None)
      .unwrap_err();
    assert_eq!(
      serde_json::from_str::<serde_json::Value>(&err).unwrap()["code"],
      "POOL_SINGLE_MEMBER"
    );
  }

  #[test]
  fn test_pool_for_proxy_and_resolve_rotate_target() {
    let temp_dir = TempDir::new().unwrap();
    let _serial = serial_guard();
    let _guard = crate::app_dirs::set_test_data_dir(temp_dir.path().to_path_buf());
    PROXY_POOL_MANAGER.reset_for_tests();

    let (pool_id, ids) = seed_pool("Find", &["f-a", "f-b"]);
    assert_eq!(
      PROXY_POOL_MANAGER.pool_for_proxy(&ids[1]).unwrap().id,
      pool_id
    );
    assert!(PROXY_POOL_MANAGER.pool_for_proxy("stray").is_none());

    let profile = profile_with_proxy(Some(ids[0].clone()));
    let target = PROXY_POOL_MANAGER.resolve_rotate_target(&profile).unwrap();
    assert_ne!(target, ids[0]);
    assert!(ids.contains(&target));

    let detached = profile_with_proxy(Some("stray".to_string()));
    let err = PROXY_POOL_MANAGER
      .resolve_rotate_target(&detached)
      .unwrap_err();
    assert_eq!(
      serde_json::from_str::<serde_json::Value>(&err).unwrap()["code"],
      "PROFILE_NOT_IN_POOL"
    );
  }

  #[test]
  fn test_launch_failover_switches_member_on_failure() {
    let temp_dir = TempDir::new().unwrap();
    let _serial = serial_guard();
    let _guard = crate::app_dirs::set_test_data_dir(temp_dir.path().to_path_buf());
    PROXY_POOL_MANAGER.reset_for_tests();

    let (_pool_id, ids) = seed_pool("Fail", &["f1", "f2"]);
    let (other_id, _) = seed_pool("Other", &["o1", "o2"]);
    let _ = other_id;

    let attempts = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let attempts_clone = attempts.clone();
    let ids_clone = ids.clone();
    let result = tokio::runtime::Runtime::new()
      .unwrap()
      .block_on(launch_with_pool_failover(
        profile_with_proxy(Some(ids[0].clone())),
        move |p| {
          let attempts = attempts_clone.clone();
          let ids = ids_clone.clone();
          Box::pin(async move {
            attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if p.proxy_id.as_deref() == Some(ids[0].as_str()) {
              Err("proxy f1 is dead".to_string())
            } else {
              Ok(p)
            }
          })
        },
        |_profile_id, proxy_id| Box::pin(async move { Ok(profile_with_proxy(Some(proxy_id))) }),
      ))
      .unwrap();

    assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 2);
    assert_eq!(result.proxy_id.as_deref(), Some(ids[1].as_str()));
  }

  #[test]
  fn test_launch_failover_exhausts_all_members() {
    let temp_dir = TempDir::new().unwrap();
    let _serial = serial_guard();
    let _guard = crate::app_dirs::set_test_data_dir(temp_dir.path().to_path_buf());
    PROXY_POOL_MANAGER.reset_for_tests();

    let (pool_id, ids) = seed_pool("DeadPool", &["d1", "d2", "d3"]);
    let _ = pool_id;

    let attempts = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let attempts_clone = attempts.clone();
    let err = tokio::runtime::Runtime::new()
      .unwrap()
      .block_on(launch_with_pool_failover(
        profile_with_proxy(Some(ids[0].clone())),
        move |p| {
          let attempts = attempts_clone.clone();
          Box::pin(async move {
            attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Err(format!("member {} is dead", p.proxy_id.unwrap_or_default()))
          })
        },
        |_profile_id, proxy_id| Box::pin(async move { Ok(profile_with_proxy(Some(proxy_id))) }),
      ))
      .unwrap_err();

    assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 3);
    assert!(
      ids.iter().any(|id| err.contains(id)),
      "exhausted error must reference one of the dead members: {err}"
    );
  }

  #[test]
  fn test_launch_without_pool_or_single_member_launches_directly() {
    let temp_dir = TempDir::new().unwrap();
    let _serial = serial_guard();
    let _guard = crate::app_dirs::set_test_data_dir(temp_dir.path().to_path_buf());
    PROXY_POOL_MANAGER.reset_for_tests();

    let (single_id, ids) = seed_pool("Solo", &["s1"]);
    let _ = single_id;

    let attempts = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let attempts_clone = attempts.clone();
    let result = tokio::runtime::Runtime::new()
      .unwrap()
      .block_on(launch_with_pool_failover(
        profile_with_proxy(Some(ids[0].clone())),
        move |p| {
          attempts_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
          Box::pin(async move { Ok(p) })
        },
        |_profile_id, proxy_id| Box::pin(async move { Ok(profile_with_proxy(Some(proxy_id))) }),
      ))
      .unwrap();
    assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(result.proxy_id.as_deref(), Some(ids[0].as_str()));
  }
}
