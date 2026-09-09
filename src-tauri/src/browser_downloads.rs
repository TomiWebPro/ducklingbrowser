//! Sandboxed browser download directories.
//!
//! Agents (interactive or scheduled) may only route downloads into directories
//! inside the app data root, or into a profile's explicitly configured
//! `download_dir`. Every path is canonicalized and prefix-checked so `..`
//! escapes and absolute outsiders are rejected before touching CDP.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::profile::BrowserProfile;

/// Default per-profile download folder: `<data>/profiles/<id>/downloads`.
pub fn default_download_dir(profile_id: &uuid::Uuid) -> PathBuf {
  crate::app_dirs::profiles_dir()
    .join(profile_id.to_string())
    .join("downloads")
}

/// App-wide fallback root for task-level download overrides.
pub fn shared_downloads_root() -> PathBuf {
  crate::app_dirs::data_dir().join("downloads")
}

/// Validate a profile-configured download dir at save time: absolute,
/// creatable, and (after canonicalization) not escaping to sensitive roots.
pub fn validate_configured_dir(raw: &str) -> Result<PathBuf, String> {
  let trimmed = raw.trim();
  if trimmed.is_empty() {
    return Err("Download folder must not be empty".to_string());
  }
  let path = PathBuf::from(trimmed);
  if !path.is_absolute() {
    return Err("Download folder must be an absolute path".to_string());
  }
  fs::create_dir_all(&path).map_err(|e| format!("Cannot create download folder: {e}"))?;
  let canonical = path
    .canonicalize()
    .map_err(|e| format!("Cannot resolve download folder: {e}"))?;
  reject_sensitive_root(&canonical)?;
  Ok(canonical)
}

fn reject_sensitive_root(canonical: &Path) -> Result<(), String> {
  // Never allow the filesystem root, home, or OS temp as a download target.
  let mut bad: Vec<PathBuf> = Vec::new();
  if let Some(home) = dirs::home_dir() {
    bad.push(home);
  }
  bad.push(std::env::temp_dir());
  #[cfg(target_os = "windows")]
  bad.push(PathBuf::from("C:\\Windows"));
  #[cfg(not(target_os = "windows"))]
  {
    bad.push(PathBuf::from("/"));
    bad.push(PathBuf::from("/etc"));
    bad.push(PathBuf::from("/bin"));
  }
  for root in bad {
    if let Ok(canon_root) = root.canonicalize() {
      if canonical == canon_root {
        return Err("Download folder must be a dedicated directory".to_string());
      }
    }
  }
  Ok(())
}

/// Resolve the effective download directory for a profile + optional
/// per-call override. The result always exists on disk.
pub fn resolve_download_dir(
  profile: &BrowserProfile,
  override_dir: Option<&str>,
) -> Result<PathBuf, String> {
  if !profile.allow_agent_downloads {
    return Err(serde_json::json!({ "code": "AGENT_DOWNLOADS_DISABLED" }).to_string());
  }
  let dir = match override_dir {
    Some(raw) if !raw.trim().is_empty() => {
      let requested = PathBuf::from(raw.trim());
      // Relative overrides resolve under the shared downloads root.
      let joined = if requested.is_absolute() {
        requested
      } else {
        shared_downloads_root().join(requested)
      };
      fs::create_dir_all(&joined).map_err(|e| format!("Cannot create download folder: {e}"))?;
      let canonical = joined
        .canonicalize()
        .map_err(|e| format!("Cannot resolve download folder: {e}"))?;
      // Overrides must stay inside the app data dir (or the profile's own
      // configured dir) — never arbitrary absolute locations.
      let data_root = crate::app_dirs::data_dir()
        .canonicalize()
        .unwrap_or_else(|_| crate::app_dirs::data_dir());
      let in_data = canonical.starts_with(&data_root);
      let in_configured = profile
        .download_dir
        .as_deref()
        .and_then(|d| PathBuf::from(d).canonicalize().ok())
        .is_some_and(|cfg| canonical.starts_with(cfg));
      if !in_data && !in_configured {
        return Err("Download folder must stay inside the app data directory".to_string());
      }
      reject_sensitive_root(&canonical)?;
      canonical
    }
    _ => match profile.download_dir.as_deref() {
      Some(cfg) if !cfg.trim().is_empty() => {
        let path = PathBuf::from(cfg.trim());
        fs::create_dir_all(&path).map_err(|e| format!("Cannot create download folder: {e}"))?;
        path
          .canonicalize()
          .map_err(|e| format!("Cannot resolve download folder: {e}"))?
      }
      _ => {
        let dir = default_download_dir(&profile.id);
        fs::create_dir_all(&dir).map_err(|e| format!("Cannot create download folder: {e}"))?;
        dir.canonicalize().unwrap_or(dir)
      }
    },
  };
  Ok(dir)
}

/// A downloaded file visible to agents.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DownloadedFile {
  pub name: String,
  pub size_bytes: u64,
  pub modified_at: u64,
}

/// List finished files (skip in-progress `.crdownload`/`.part` temporaries).
pub fn list_downloads(dir: &Path) -> Vec<DownloadedFile> {
  let mut out = Vec::new();
  let Ok(entries) = fs::read_dir(dir) else {
    return out;
  };
  for entry in entries.flatten() {
    let name = entry.file_name().to_string_lossy().to_string();
    if name.ends_with(".crdownload") || name.ends_with(".part") || name.ends_with(".tmp") {
      continue;
    }
    let meta = entry.metadata().ok();
    let path = entry.path();
    if !path.is_file() {
      continue;
    }
    out.push(DownloadedFile {
      name,
      size_bytes: meta.as_ref().map(|m| m.len()).unwrap_or(0),
      modified_at: meta
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0),
    });
  }
  out.sort_by_key(|a| std::cmp::Reverse(a.modified_at));
  out
}

/// Wait until at least one new stable file appears (size unchanged across two
/// polls and no temp suffix). Returns the new files.
pub async fn wait_for_new_downloads(
  dir: &Path,
  known: &std::collections::HashSet<String>,
  timeout: Duration,
) -> Vec<DownloadedFile> {
  let deadline = tokio::time::Instant::now() + timeout;
  let mut last_snapshot: Vec<DownloadedFile> = Vec::new();
  loop {
    let current = list_downloads(dir);
    let fresh: Vec<DownloadedFile> = current
      .iter()
      .filter(|f| !known.contains(&f.name))
      .cloned()
      .collect();
    // Stability: same set with same sizes as the previous poll.
    if !fresh.is_empty()
      && fresh.len() == last_snapshot.len()
      && fresh
        .iter()
        .zip(last_snapshot.iter())
        .all(|(a, b)| a.name == b.name && a.size_bytes == b.size_bytes)
    {
      return fresh;
    }
    last_snapshot = fresh;
    if tokio::time::Instant::now() >= deadline {
      return last_snapshot;
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn test_profile(dir_name: &str) -> (BrowserProfile, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    let _guard = crate::app_dirs::set_test_data_dir(tmp.path().join(dir_name));
    let profile = BrowserProfile {
      allow_agent_downloads: true,
      ..Default::default()
    };
    (profile, tmp)
  }

  #[test]
  fn default_dir_resolves_and_exists() {
    let (profile, _tmp) = test_profile("dl-default");
    let dir = resolve_download_dir(&profile, None).unwrap();
    assert!(dir.is_dir());
    assert!(dir.ends_with("downloads"));
  }

  #[test]
  fn disabled_profile_rejects_with_structured_code() {
    let (mut profile, _tmp) = test_profile("dl-disabled");
    profile.allow_agent_downloads = false;
    let err = resolve_download_dir(&profile, None).unwrap_err();
    assert_eq!(
      serde_json::from_str::<serde_json::Value>(&err).unwrap()["code"],
      "AGENT_DOWNLOADS_DISABLED"
    );
  }

  #[test]
  fn absolute_outsider_override_is_rejected() {
    let (profile, _tmp) = test_profile("dl-outsider");
    let outside = if cfg!(target_os = "windows") {
      "C:\\Windows\\Temp\\duckling-evil"
    } else {
      "/tmp/duckling-evil"
    };
    assert!(resolve_download_dir(&profile, Some(outside)).is_err());
  }

  #[test]
  fn dotdot_escape_stays_inside() {
    let (profile, _tmp) = test_profile("dl-dotdot");
    // `..` is canonicalized: sub/../sub2 lands inside the data root.
    let dir = resolve_download_dir(&profile, Some("sub/../sub2")).unwrap();
    assert!(dir.is_dir());
    // Absolute escape outside the data root is rejected.
    let evil = if cfg!(target_os = "windows") {
      "C:\\Windows"
    } else {
      "/etc"
    };
    assert!(resolve_download_dir(&profile, Some(evil)).is_err());
  }

  #[test]
  fn relative_override_nests_under_shared_root() {
    let (profile, _tmp) = test_profile("dl-relative");
    let dir = resolve_download_dir(&profile, Some("cron-task-1")).unwrap();
    assert!(dir.ends_with("cron-task-1"));
    assert!(dir.is_dir());
  }

  #[test]
  fn list_skips_in_progress_files() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("a.pdf"), b"data").unwrap();
    fs::write(tmp.path().join("b.crdownload"), b"partial").unwrap();
    fs::write(tmp.path().join("c.part"), b"partial").unwrap();
    let listed = list_downloads(tmp.path());
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].name, "a.pdf");
    assert_eq!(listed[0].size_bytes, 4);
  }

  #[test]
  fn wait_returns_stable_new_files() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
      let tmp = tempfile::tempdir().unwrap();
      fs::write(tmp.path().join("old.txt"), b"old").unwrap();
      let known: std::collections::HashSet<String> = list_downloads(tmp.path())
        .iter()
        .map(|f| f.name.clone())
        .collect();
      fs::write(tmp.path().join("new.txt"), b"hello").unwrap();
      let found = wait_for_new_downloads(tmp.path(), &known, Duration::from_secs(5)).await;
      assert_eq!(found.len(), 1);
      assert_eq!(found[0].name, "new.txt");
    });
  }

  #[test]
  fn configured_dir_must_be_absolute() {
    assert!(validate_configured_dir("relative/path").is_err());
    assert!(validate_configured_dir("").is_err());
  }
}
