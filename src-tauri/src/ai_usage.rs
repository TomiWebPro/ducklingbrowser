use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;

use crate::llm::ChatUsage;

/// Per-bucket AI usage totals persisted to a local JSON file.
/// Buckets are keyed by saved-key id, profile id, and provider id.
///
/// Serialized names are snake_case (`by_key`, `prompt_tokens`, …) because
/// every consumer — the frontend and the e2e suite — reads those exact keys.
/// The `alias` entries keep older camelCase files (`byKey`, `promptTokens`,
/// …) readable so existing ledgers survive the upgrade.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct UsageEntry {
  #[serde(default, alias = "promptTokens")]
  pub prompt_tokens: u64,
  #[serde(default, alias = "completionTokens")]
  pub completion_tokens: u64,
  #[serde(default, alias = "totalTokens")]
  pub total_tokens: u64,
  #[serde(default, skip_serializing_if = "Option::is_none", alias = "costUsd")]
  pub cost_usd: Option<f64>,
  #[serde(default)]
  pub calls: u64,
  #[serde(default, skip_serializing_if = "Option::is_none", alias = "lastUsed")]
  pub last_used: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UsageFile {
  #[serde(default, alias = "byKey")]
  pub by_key: HashMap<String, UsageEntry>,
  #[serde(default, alias = "byProfile")]
  pub by_profile: HashMap<String, UsageEntry>,
  #[serde(default, alias = "byProvider")]
  pub by_provider: HashMap<String, UsageEntry>,
}

fn usage_file_path() -> std::path::PathBuf {
  crate::app_dirs::data_subdir().join("ai_usage.json")
}

fn load_file() -> UsageFile {
  let path = usage_file_path();
  if !path.exists() {
    return UsageFile::default();
  }
  fs::read_to_string(&path)
    .ok()
    .and_then(|c| serde_json::from_str(&c).ok())
    .unwrap_or_default()
}

fn persist_file(data: &UsageFile) {
  let path = usage_file_path();
  if let Some(parent) = path.parent() {
    let _ = fs::create_dir_all(parent);
  }
  if let Ok(json) = serde_json::to_string_pretty(data) {
    let _ = fs::write(path, json);
  }
}

fn add(entry: &mut UsageEntry, usage: &ChatUsage, now: u64) {
  entry.prompt_tokens = entry
    .prompt_tokens
    .saturating_add(usage.prompt_tokens as u64);
  entry.completion_tokens = entry
    .completion_tokens
    .saturating_add(usage.completion_tokens as u64);
  entry.total_tokens = entry.total_tokens.saturating_add(usage.total_tokens as u64);
  if let Some(c) = usage.cost {
    if c.is_finite() && c >= 0.0 {
      entry.cost_usd = Some(entry.cost_usd.unwrap_or(0.0) + c);
    }
  }
  entry.calls = entry.calls.saturating_add(1);
  entry.last_used = Some(now);
}

/// Record one LLM call against the local usage ledger. Never fails the
/// caller — persistence errors are logged only.
pub fn record_usage(
  key_id: Option<&str>,
  provider: Option<&str>,
  profile_id: Option<&str>,
  usage: &ChatUsage,
) {
  let mut data = load_file();
  let now = crate::proxy_manager::now_secs();
  if let Some(k) = key_id.filter(|k| !k.trim().is_empty()) {
    add(data.by_key.entry(k.to_string()).or_default(), usage, now);
  }
  if let Some(p) = profile_id.filter(|p| !p.trim().is_empty()) {
    add(
      data.by_profile.entry(p.to_string()).or_default(),
      usage,
      now,
    );
  }
  if let Some(p) = provider.filter(|p| !p.trim().is_empty()) {
    add(
      data.by_provider.entry(p.to_string()).or_default(),
      usage,
      now,
    );
  }
  persist_file(&data);
}

#[tauri::command]
pub fn ai_usage_stats() -> Result<UsageFile, String> {
  Ok(load_file())
}

#[tauri::command]
pub fn ai_usage_reset() -> Result<(), String> {
  persist_file(&UsageFile::default());
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn ledger_accumulates_tokens_and_optional_cost() {
    let tmp = tempfile::tempdir().unwrap();
    let _guard = crate::app_dirs::set_test_data_dir(tmp.path().to_path_buf());

    record_usage(
      Some("k1"),
      Some("openrouter"),
      Some("p1"),
      &ChatUsage {
        prompt_tokens: 10,
        completion_tokens: 5,
        total_tokens: 15,
        cost: Some(0.001),
      },
    );
    record_usage(
      Some("k1"),
      Some("openrouter"),
      Some("p1"),
      &ChatUsage {
        prompt_tokens: 4,
        completion_tokens: 2,
        total_tokens: 6,
        cost: None,
      },
    );

    let stats = load_file();
    let key = &stats.by_key["k1"];
    assert_eq!(key.total_tokens, 21);
    assert_eq!(key.calls, 2);
    assert!((key.cost_usd.unwrap() - 0.001).abs() < 1e-9);
    assert_eq!(stats.by_profile["p1"].prompt_tokens, 14);
    assert_eq!(stats.by_provider["openrouter"].completion_tokens, 7);
  }

  #[test]
  fn usage_file_serializes_snake_case_for_frontend_consumers() {
    let mut file = UsageFile::default();
    file.by_profile.insert(
      "p1".to_string(),
      UsageEntry {
        prompt_tokens: 10,
        completion_tokens: 5,
        total_tokens: 15,
        cost_usd: Some(0.001),
        calls: 2,
        last_used: Some(1),
      },
    );
    let json: serde_json::Value = serde_json::to_value(&file).expect("usage file serializes");
    let entry = &json["by_profile"]["p1"];
    assert_eq!(entry["prompt_tokens"], 10);
    assert_eq!(entry["completion_tokens"], 5);
    assert_eq!(entry["total_tokens"], 15);
    assert_eq!(entry["calls"], 2);
    assert!(json.get("byProfile").is_none());
  }

  #[test]
  fn legacy_camel_case_ledger_still_deserializes() {
    let legacy = serde_json::json!({
      "byKey": {},
      "byProfile": {
        "p1": {
          "promptTokens": 3,
          "completionTokens": 1,
          "totalTokens": 4,
          "calls": 1
        }
      },
      "byProvider": {}
    });
    let file: UsageFile = serde_json::from_value(legacy).expect("legacy ledger parses");
    assert_eq!(file.by_profile["p1"].prompt_tokens, 3);
    assert_eq!(file.by_profile["p1"].total_tokens, 4);
  }
}
