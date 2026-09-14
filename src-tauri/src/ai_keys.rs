use std::fs;
use std::path::PathBuf;

use aes_gcm::aead::Aead;
use aes_gcm::{Aes256Gcm, Key, KeyInit, Nonce};
use argon2::{password_hash::SaltString, Argon2, PasswordHasher};
use rand::RngExt;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AiProvider {
  Anthropic,
  Openai,
  Groq,
  Xai,
  Google,
  Openrouter,
  Opencode,
  Custom,
}

impl AiProvider {
  pub fn as_str(&self) -> &'static str {
    match self {
      AiProvider::Anthropic => "anthropic",
      AiProvider::Openai => "openai",
      AiProvider::Groq => "groq",
      AiProvider::Xai => "xai",
      AiProvider::Google => "google",
      AiProvider::Openrouter => "openrouter",
      AiProvider::Opencode => "opencode",
      AiProvider::Custom => "custom",
    }
  }

  /// Default chat-completions endpoint for OpenAI-compatible providers.
  /// Non-compatible providers return None.
  pub fn default_endpoint(&self) -> Option<&'static str> {
    match self {
      AiProvider::Opencode => Some("https://opencode.ai/zen/go/v1/chat/completions"),
      AiProvider::Custom => None,
      AiProvider::Openai => Some("https://api.openai.com/v1/chat/completions"),
      AiProvider::Groq => Some("https://api.groq.com/openai/v1/chat/completions"),
      AiProvider::Xai => Some("https://api.x.ai/v1/chat/completions"),
      AiProvider::Openrouter => Some("https://openrouter.ai/api/v1/chat/completions"),
      _ => None,
    }
  }

  /// True for providers speaking the OpenAI chat-completions wire format.
  pub fn is_openai_compatible(&self) -> bool {
    matches!(
      self,
      AiProvider::Openai
        | AiProvider::Groq
        | AiProvider::Xai
        | AiProvider::Openrouter
        | AiProvider::Opencode
        | AiProvider::Custom
    )
  }
}

impl std::str::FromStr for AiProvider {
  type Err = ();

  fn from_str(s: &str) -> Result<Self, Self::Err> {
    match s {
      "anthropic" => Ok(AiProvider::Anthropic),
      "openai" => Ok(AiProvider::Openai),
      "groq" => Ok(AiProvider::Groq),
      "xai" | "grok" => Ok(AiProvider::Xai),
      "google" => Ok(AiProvider::Google),
      "openrouter" => Ok(AiProvider::Openrouter),
      "opencode" => Ok(AiProvider::Opencode),
      "custom" => Ok(AiProvider::Custom),
      _ => Err(()),
    }
  }
}

/// Normalize + validate a user-supplied base endpoint URL.
/// Accepts a full chat-completions URL or a `/v1` base; rejects
/// non-http(s) schemes, missing hosts, and overlong values.
pub fn normalize_endpoint(raw: &str) -> Result<String, String> {
  let trimmed = raw.trim();
  if trimmed.is_empty() {
    return Err(invalid("endpoint must not be empty"));
  }
  if trimmed.len() > 2000 {
    return Err(invalid("endpoint URL is too long"));
  }
  let parsed: url::Url = trimmed
    .parse()
    .map_err(|_| invalid("endpoint must be a valid http(s) URL"))?;
  match parsed.scheme() {
    "http" | "https" => {}
    _ => return Err(invalid("endpoint must start with http:// or https://")),
  }
  if parsed.host_str().is_none_or(|h| h.is_empty()) {
    return Err(invalid("endpoint must include a host"));
  }
  Ok(trimmed.trim_end_matches('/').to_string())
}

/// Full record kept inside the encrypted vault file only. The plaintext key
/// must never cross the Tauri boundary.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AiKeyRecord {
  pub id: String,
  pub provider: String,
  pub name: String,
  pub model: String,
  pub key: String,
  pub created_at: String,
  /// Optional per-key endpoint override (normalized base URL).
  /// `None` = use the provider default. Legacy records deserialize to None.
  #[serde(default)]
  pub endpoint: Option<String>,
}

/// Safe shape returned to the frontend: the key is masked.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AiKeyInfo {
  pub id: String,
  pub provider: String,
  pub name: String,
  pub model: String,
  pub masked_key: String,
  pub created_at: String,
  #[serde(default)]
  pub endpoint: Option<String>,
}

pub fn mask_key(key: &str) -> String {
  let chars: Vec<char> = key.chars().collect();
  if chars.len() <= 8 {
    return "***".to_string();
  }
  let head: String = chars[..3].iter().collect();
  let tail: String = chars[chars.len() - 4..].iter().collect();
  format!("{head}***{tail}")
}

fn code_error(code: &str, params: serde_json::Value) -> String {
  serde_json::json!({ "code": code, "params": params }).to_string()
}

fn invalid(detail: &str) -> String {
  code_error("AI_KEY_INVALID", serde_json::json!({ "detail": detail }))
}

fn not_found(id: &str) -> String {
  code_error("AI_KEY_NOT_FOUND", serde_json::json!({ "id": id }))
}

const VAULT_HEADER: &[u8; 5] = b"DBKEY";

fn vault_password() -> String {
  env!("DUCKLING_BROWSER_VAULT_PASSWORD").to_string()
}

/// Encrypt `payload` into the version-2 Argon2 + AES-256-GCM file layout used
/// by the token vault (header + version + salt-len + salt + nonce + len + body).
fn encrypt_payload(payload: &[u8]) -> Result<Vec<u8>, String> {
  let salt_bytes: [u8; 16] = rand::rng().random();
  let salt =
    SaltString::encode_b64(&salt_bytes).map_err(|e| format!("Failed to encode salt: {e}"))?;
  let argon2 = Argon2::default();
  let password_hash = argon2
    .hash_password(vault_password().as_bytes(), &salt)
    .map_err(|e| format!("Argon2 key derivation failed: {e}"))?;
  let hash_value = password_hash.hash.unwrap();
  let hash_bytes = hash_value.as_bytes();
  let key_bytes: [u8; 32] = hash_bytes[..32]
    .try_into()
    .map_err(|_| "Invalid key length")?;
  let key = Key::<Aes256Gcm>::from(key_bytes);
  let cipher = Aes256Gcm::new(&key);
  let nonce_bytes: [u8; 12] = rand::rng().random();
  let nonce = Nonce::from(nonce_bytes);
  let ciphertext = cipher
    .encrypt(&nonce, payload)
    .map_err(|e| format!("Encryption failed: {e}"))?;

  let mut file_data = Vec::new();
  file_data.extend_from_slice(VAULT_HEADER);
  file_data.push(2u8);
  let salt_str = salt.as_str();
  file_data.push(salt_str.len() as u8);
  file_data.extend_from_slice(salt_str.as_bytes());
  file_data.extend_from_slice(&nonce);
  file_data.extend_from_slice(&(ciphertext.len() as u32).to_le_bytes());
  file_data.extend_from_slice(&ciphertext);
  Ok(file_data)
}

fn decrypt_payload(file_data: &[u8]) -> Result<Vec<u8>, String> {
  if file_data.len() < 6 || &file_data[0..5] != VAULT_HEADER {
    return Err("invalid vault header".to_string());
  }
  let version = file_data[5];
  if version != 2 {
    return Err("unsupported vault version".to_string());
  }

  let mut offset = 6;
  if offset >= file_data.len() {
    return Err("truncated vault".to_string());
  }
  let salt_len = file_data[offset] as usize;
  offset += 1;

  if offset + salt_len > file_data.len() {
    return Err("truncated vault salt".to_string());
  }
  let salt_bytes = &file_data[offset..offset + salt_len];
  let salt_str = std::str::from_utf8(salt_bytes).map_err(|_| "Invalid salt encoding")?;
  let salt = SaltString::from_b64(salt_str).map_err(|_| "Invalid salt format")?;
  offset += salt_len;

  if offset + 12 > file_data.len() {
    return Err("truncated vault nonce".to_string());
  }
  let nonce_bytes: [u8; 12] = file_data[offset..offset + 12]
    .try_into()
    .map_err(|_| "Invalid nonce length")?;
  let nonce = Nonce::from(nonce_bytes);
  offset += 12;

  if offset + 4 > file_data.len() {
    return Err("truncated vault length".to_string());
  }
  let ciphertext_len =
    u32::from_le_bytes(file_data[offset..offset + 4].try_into().unwrap()) as usize;
  offset += 4;

  if offset + ciphertext_len > file_data.len() {
    return Err("truncated vault ciphertext".to_string());
  }
  let ciphertext = &file_data[offset..offset + ciphertext_len];

  let argon2 = Argon2::default();
  let password_hash = argon2
    .hash_password(vault_password().as_bytes(), &salt)
    .map_err(|e| format!("Argon2 key derivation failed: {e}"))?;
  let hash_value = password_hash.hash.unwrap();
  let hash_bytes = hash_value.as_bytes();
  let key_bytes: [u8; 32] = hash_bytes[..32]
    .try_into()
    .map_err(|_| "Invalid key length")?;
  let key = Key::<Aes256Gcm>::from(key_bytes);
  let cipher = Aes256Gcm::new(&key);
  cipher
    .decrypt(&nonce, ciphertext)
    .map_err(|_| "Decryption failed".to_string())
}

fn vault_file() -> PathBuf {
  crate::app_dirs::settings_dir().join("ai_keys.dat")
}

fn load_records() -> Vec<AiKeyRecord> {
  let file = vault_file();
  if !file.exists() {
    return Vec::new();
  }
  let Ok(data) = fs::read(&file) else {
    return Vec::new();
  };
  decrypt_payload(&data)
    .and_then(|plain| {
      serde_json::from_slice::<Vec<AiKeyRecord>>(&plain)
        .map_err(|e| format!("Failed to parse vault: {e}"))
    })
    .unwrap_or_default()
}

fn persist_records(records: &[AiKeyRecord]) -> Result<(), String> {
  let dir = crate::app_dirs::settings_dir();
  fs::create_dir_all(&dir).map_err(|e| format!("Failed to create settings dir: {e}"))?;
  let payload =
    serde_json::to_vec(records).map_err(|e| format!("Failed to serialize vault: {e}"))?;
  let encrypted = encrypt_payload(&payload)?;
  let file = vault_file();
  fs::write(&file, encrypted).map_err(|e| format!("Failed to write vault: {e}"))?;
  crate::app_dirs::restrict_to_owner(&file);
  Ok(())
}

fn now_iso() -> String {
  chrono::Utc::now().to_rfc3339()
}

fn to_info(record: &AiKeyRecord) -> AiKeyInfo {
  AiKeyInfo {
    id: record.id.clone(),
    provider: record.provider.clone(),
    name: record.name.clone(),
    model: record.model.clone(),
    masked_key: mask_key(&record.key),
    created_at: record.created_at.clone(),
    endpoint: record.endpoint.clone(),
  }
}

pub fn list_keys() -> Result<Vec<AiKeyInfo>, String> {
  Ok(load_records().iter().map(to_info).collect())
}

/// Crate-internal: returns the decrypted record (including the plaintext key)
/// for the LLM clients. Never expose the result to the frontend.
pub fn get_key(id: &str) -> Result<Option<AiKeyRecord>, String> {
  Ok(load_records().into_iter().find(|r| r.id == id))
}

/// Crate-internal: all decrypted records, used to pick a key when the chat
/// call does not specify one.
pub fn all_records() -> Vec<AiKeyRecord> {
  load_records()
}

pub fn save_key(
  provider: &str,
  name: &str,
  model: &str,
  key: &str,
  endpoint: Option<&str>,
) -> Result<AiKeyInfo, String> {
  let provider_parsed: AiProvider = provider
    .parse()
    .map_err(|_| invalid(&format!("unknown provider '{provider}'")))?;
  // OpenCode Go needs only a key: default an empty name so callers can omit it.
  let effective_name = if name.trim().is_empty() {
    if provider_parsed == AiProvider::Opencode {
      "OpenCode Go".to_string()
    } else {
      return Err(crate::backend_error("NAME_CANNOT_BE_EMPTY"));
    }
  } else {
    name.trim().to_string()
  };
  if model.trim().is_empty() {
    return Err(invalid("model must not be empty"));
  }
  if key.trim().is_empty() {
    return Err(invalid("key must not be empty"));
  }

  // Normalize the endpoint override. Custom requires one; opencode falls
  // back to the Go subscription default; other providers accept an optional override.
  let normalized_endpoint: Option<String> = match endpoint {
    Some(raw) if !raw.trim().is_empty() => Some(normalize_endpoint(raw)?),
    _ => None,
  };
  if provider_parsed == AiProvider::Custom && normalized_endpoint.is_none() {
    return Err(invalid("custom provider requires an endpoint URL"));
  }
  if !provider_parsed.is_openai_compatible() && normalized_endpoint.is_some() {
    return Err(invalid(
      "endpoint overrides are only supported for OpenAI-compatible providers",
    ));
  }

  let mut records = load_records();
  let now = now_iso();
  let record = if let Some(existing) = records.iter_mut().find(|r| r.name == effective_name) {
    existing.provider = provider_parsed.as_str().to_string();
    existing.model = model.trim().to_string();
    existing.key = key.trim().to_string();
    existing.endpoint = normalized_endpoint;
    existing.clone()
  } else {
    let record = AiKeyRecord {
      id: Uuid::new_v4().to_string(),
      provider: provider_parsed.as_str().to_string(),
      name: effective_name,
      model: model.trim().to_string(),
      key: key.trim().to_string(),
      created_at: now,
      endpoint: normalized_endpoint,
    };
    records.push(record.clone());
    record
  };

  persist_records(&records)?;
  Ok(to_info(&record))
}

pub fn delete_key(id: &str) -> Result<(), String> {
  let mut records = load_records();
  let before = records.len();
  records.retain(|r| r.id != id);
  if records.len() == before {
    return Err(not_found(id));
  }
  persist_records(&records)
}

/// Derive the models-list probe URL for OpenAI-compatible providers.
/// A full `/chat/completions` override is mapped to `/models`; a `/v1` base
/// gets `/models` appended; a bare host without a version path falls back
/// to `/v1/models` so live catalog fetch still works.
fn compat_models_url(endpoint_override: Option<&str>, fallback: &str) -> String {
  let base = endpoint_override.unwrap_or(fallback);
  let trimmed = base.trim_end_matches('/');
  if let Some(root) = trimmed.strip_suffix("/chat/completions") {
    format!("{root}/models")
  } else if trimmed.ends_with("/models") {
    trimmed.to_string()
  } else if trimmed.ends_with("/v1") {
    format!("{trimmed}/models")
  } else {
    format!("{trimmed}/v1/models")
  }
}

async fn probe(
  provider: AiProvider,
  key: &str,
  model: &str,
  endpoint_override: Option<&str>,
) -> Result<serde_json::Value, String> {
  let client = reqwest::Client::builder()
    .timeout(std::time::Duration::from_secs(10))
    .build()
    .map_err(|e| format!("Failed to build HTTP client: {e}"))?;

  let (ok, detail) = match provider {
    AiProvider::Anthropic => {
      let response = client
        .post("https://api.anthropic.com/v1/messages")
        .header("x-api-key", key)
        .header("anthropic-version", "2023-06-01")
        .json(&serde_json::json!({
          "model": model,
          "max_tokens": 1,
          "messages": [{ "role": "user", "content": "ping" }]
        }))
        .send()
        .await;
      probe_outcome(response)
    }
    AiProvider::Openai => {
      let url = compat_models_url(
        endpoint_override,
        "https://api.openai.com/v1/chat/completions",
      );
      let response = client.get(url).bearer_auth(key).send().await;
      probe_outcome(response)
    }
    AiProvider::Groq => {
      let url = compat_models_url(
        endpoint_override,
        "https://api.groq.com/openai/v1/chat/completions",
      );
      let response = client.get(url).bearer_auth(key).send().await;
      probe_outcome(response)
    }
    AiProvider::Xai => {
      let url = compat_models_url(endpoint_override, "https://api.x.ai/v1/chat/completions");
      let response = client.get(url).bearer_auth(key).send().await;
      probe_outcome(response)
    }
    AiProvider::Openrouter => {
      let url = compat_models_url(
        endpoint_override,
        "https://openrouter.ai/api/v1/chat/completions",
      );
      let response = client
        .get(url)
        .header("HTTP-Referer", "https://ducklingbrowser.com")
        .header("X-Title", "Duckling Browser")
        .bearer_auth(key)
        .send()
        .await;
      probe_outcome(response)
    }
    AiProvider::Opencode => {
      let url = compat_models_url(
        endpoint_override,
        "https://opencode.ai/zen/go/v1/chat/completions",
      );
      let response = client.get(url).bearer_auth(key).send().await;
      probe_outcome(response)
    }
    AiProvider::Custom => {
      let Some(override_url) = endpoint_override else {
        return Ok(
          serde_json::json!({ "ok": false, "detail": "custom provider requires an endpoint URL" }),
        );
      };
      let url = compat_models_url(Some(override_url), override_url);
      let response = client.get(url).bearer_auth(key).send().await;
      probe_outcome(response)
    }
    AiProvider::Google => {
      let response = client
        .get(format!(
          "https://generativelanguage.googleapis.com/v1beta/models?key={key}"
        ))
        .send()
        .await;
      probe_outcome(response)
    }
  }
  .await;

  Ok(serde_json::json!({ "ok": ok, "detail": detail }))
}

async fn probe_outcome(response: Result<reqwest::Response, reqwest::Error>) -> (bool, String) {
  match response {
    Ok(resp) => {
      let status = resp.status();
      if status.is_success() {
        (true, "ok".to_string())
      } else {
        let body = resp
          .text()
          .await
          .unwrap_or_default()
          .chars()
          .take(200)
          .collect::<String>();
        (false, format!("HTTP {status}: {body}"))
      }
    }
    Err(e) => (false, format!("Could not reach provider: {e}")),
  }
}

/// Cap for model suggestions returned to the picker. Large catalogs
/// (OpenRouter serves 400+ text models) are returned in full: the picker is
/// a scrollable dropdown with a type-to-filter field, so truncation would
/// only hide models the user could have picked. The `/models` endpoints
/// return the whole list without pagination, so one fetch is enough.
const MAX_MODEL_SUGGESTIONS: usize = 500;

/// Substrings marking non-chat models (audio, image, embeddings, moderation,
/// safety guard) that would only clutter the model picker.
const NON_CHAT_MODEL_HINTS: &[&str] = &[
  "whisper",
  "tts",
  "dall-e",
  "embed",
  "moderation",
  "transcribe",
  "guard",
];

fn is_chat_model(id: &str) -> bool {
  let lower = id.to_lowercase();
  !NON_CHAT_MODEL_HINTS.iter().any(|hint| lower.contains(hint))
}

/// Extract string ids from a JSON array of objects, optionally stripping a
/// prefix (Google returns `models/<id>` in `name`).
fn extract_ids(items: &[serde_json::Value], key: &str, strip_prefix: Option<&str>) -> Vec<String> {
  items
    .iter()
    .filter_map(|item| {
      item.get(key)?.as_str().map(|s| match strip_prefix {
        Some(prefix) => s.strip_prefix(prefix).unwrap_or(s).to_string(),
        None => s.to_string(),
      })
    })
    .collect()
}

/// Deduplicate, drop non-chat and blank ids, and cap the suggestion count.
fn finalize_models(ids: Vec<String>) -> Vec<String> {
  let mut seen = std::collections::HashSet::new();
  ids
    .into_iter()
    .map(|s| s.trim().to_string())
    .filter(|s| !s.is_empty() && is_chat_model(s) && seen.insert(s.clone()))
    .take(MAX_MODEL_SUGGESTIONS)
    .collect()
}

/// Fetch the live model catalog for a provider. Unreachable endpoints, auth
/// failures, and unexpected payloads resolve to an empty list so the caller
/// falls back to the static suggestions; only invalid input is an error.
async fn fetch_models(
  provider: AiProvider,
  key: Option<&str>,
  endpoint_override: Option<&str>,
) -> Result<Vec<String>, String> {
  let client = reqwest::Client::builder()
    .timeout(std::time::Duration::from_secs(10))
    .build()
    .map_err(|e| format!("Failed to build HTTP client: {e}"))?;
  let clean_key = key.filter(|k| !k.trim().is_empty());

  let response = match provider {
    AiProvider::Anthropic => {
      let Some(k) = clean_key else {
        return Ok(Vec::new());
      };
      client
        .get("https://api.anthropic.com/v1/models")
        .header("x-api-key", k)
        .header("anthropic-version", "2023-06-01")
        .send()
        .await
    }
    AiProvider::Google => {
      let Some(k) = clean_key else {
        return Ok(Vec::new());
      };
      client
        .get(format!(
          "https://generativelanguage.googleapis.com/v1beta/models?key={k}"
        ))
        .send()
        .await
    }
    AiProvider::Openai
    | AiProvider::Groq
    | AiProvider::Xai
    | AiProvider::Openrouter
    | AiProvider::Opencode
    | AiProvider::Custom => {
      let base = match endpoint_override {
        Some(o) => o.to_string(),
        None => match provider.default_endpoint() {
          Some(d) => d.to_string(),
          // Custom without an endpoint has no catalog to query.
          None => return Ok(Vec::new()),
        },
      };
      let url = compat_models_url(Some(&base), &base);
      let mut request = client.get(url);
      if provider == AiProvider::Openrouter {
        request = request
          .header("HTTP-Referer", "https://ducklingbrowser.com")
          .header("X-Title", "Duckling Browser");
      }
      if let Some(k) = clean_key {
        request = request.bearer_auth(k);
      }
      request.send().await
    }
  };

  let Ok(resp) = response else {
    return Ok(Vec::new());
  };
  if !resp.status().is_success() {
    return Ok(Vec::new());
  }
  let Ok(body) = resp.json::<serde_json::Value>().await else {
    return Ok(Vec::new());
  };
  let ids = match provider {
    AiProvider::Anthropic => body
      .get("data")
      .and_then(|d| d.as_array())
      .map(|items| extract_ids(items, "id", None))
      .unwrap_or_default(),
    AiProvider::Google => body
      .get("models")
      .and_then(|d| d.as_array())
      .map(|items| extract_ids(items, "name", Some("models/")))
      .unwrap_or_default(),
    _ => body
      .get("data")
      .and_then(|d| d.as_array())
      .map(|items| extract_ids(items, "id", None))
      .unwrap_or_default(),
  };
  Ok(finalize_models(ids))
}

#[tauri::command]
pub fn ai_keys_list() -> Result<Vec<AiKeyInfo>, String> {
  list_keys()
}

#[tauri::command]
pub fn ai_keys_save(
  provider: String,
  name: String,
  model: String,
  key: String,
  endpoint: Option<String>,
) -> Result<AiKeyInfo, String> {
  save_key(&provider, &name, &model, &key, endpoint.as_deref())
}

#[tauri::command]
pub fn ai_keys_delete(id: String) -> Result<(), String> {
  delete_key(&id)
}

#[tauri::command]
pub async fn ai_keys_test(
  provider: String,
  model: String,
  key: Option<String>,
  id: Option<String>,
  endpoint: Option<String>,
) -> Result<serde_json::Value, String> {
  let provider_parsed: AiProvider = provider
    .parse()
    .map_err(|_| invalid(&format!("unknown provider '{provider}'")))?;
  let (plaintext, stored_endpoint) = match key {
    Some(k) if !k.trim().is_empty() => (k.trim().to_string(), None),
    _ => {
      let id = id.ok_or_else(|| invalid("provide a key or a saved key id"))?;
      let record = get_key(&id)?.ok_or_else(|| not_found(&id))?;
      (record.key, record.endpoint)
    }
  };
  // Explicit endpoint wins; otherwise fall back to the saved record's.
  let effective_endpoint = match endpoint {
    Some(e) if !e.trim().is_empty() => Some(normalize_endpoint(&e)?),
    _ => stored_endpoint,
  };
  probe(
    provider_parsed,
    &plaintext,
    &model,
    effective_endpoint.as_deref(),
  )
  .await
}

#[tauri::command]
pub async fn ai_keys_models(
  provider: String,
  key: Option<String>,
  id: Option<String>,
  endpoint: Option<String>,
) -> Result<Vec<String>, String> {
  let provider_parsed: AiProvider = provider
    .parse()
    .map_err(|_| invalid(&format!("unknown provider '{provider}'")))?;
  // Explicit key wins, otherwise fall back to the saved record's key/endpoint.
  // Neither is fine for public catalogs (OpenCode Go, OpenRouter).
  let (key, stored_endpoint) = match key {
    Some(k) if !k.trim().is_empty() => (Some(k.trim().to_string()), None),
    _ => match id {
      Some(given) if !given.trim().is_empty() => {
        let record = get_key(given.trim())?.ok_or_else(|| not_found(given.trim()))?;
        (Some(record.key), record.endpoint)
      }
      _ => (None, None),
    },
  };
  // Explicit endpoint wins; otherwise fall back to the saved record's.
  let effective_endpoint = match endpoint {
    Some(e) if !e.trim().is_empty() => Some(normalize_endpoint(&e)?),
    _ => stored_endpoint,
  };
  fetch_models(
    provider_parsed,
    key.as_deref(),
    effective_endpoint.as_deref(),
  )
  .await
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn mask_key_hides_middle() {
    assert_eq!(mask_key("sk-abcdefghijklmnop"), "sk-***mnop");
    assert_eq!(mask_key("short"), "***");
    assert_eq!(mask_key(""), "***");
  }

  #[test]
  fn vault_roundtrip_preserves_payload() {
    let payload = b"{\"id\":\"abc\",\"provider\":\"openai\"}";
    let encrypted = encrypt_payload(payload).unwrap();
    assert_eq!(&encrypted[0..5], VAULT_HEADER);
    let decrypted = decrypt_payload(&encrypted).unwrap();
    assert_eq!(decrypted, payload);
  }

  #[test]
  fn vault_rejects_bad_header_and_version() {
    let mut data = encrypt_payload(b"x").unwrap();
    data[0] = b'X';
    assert!(decrypt_payload(&data).is_err());
    let mut data = encrypt_payload(b"x").unwrap();
    data[5] = 9;
    assert!(decrypt_payload(&data).is_err());
    assert!(decrypt_payload(&[]).is_err());
  }

  #[test]
  fn store_roundtrip_with_temp_settings_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let _guard = crate::app_dirs::set_test_data_dir(tmp.path().to_path_buf());

    assert!(list_keys().unwrap().is_empty());

    let saved = save_key("openai", "Main key", "gpt-4o-mini", "sk-test-123456", None).unwrap();
    assert_eq!(saved.masked_key, "sk-***3456");
    assert!(saved.endpoint.is_none());
    assert!(get_key(&saved.id).unwrap().is_some());

    let listed = list_keys().unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].provider, "openai");

    assert!(save_key("bad-provider", "x", "model", "key", None).is_err());
    assert!(save_key("openai", "", "model", "key", None).is_err());

    delete_key(&saved.id).unwrap();
    assert!(list_keys().unwrap().is_empty());
    assert!(delete_key(&saved.id).is_err());
  }

  #[test]
  fn provider_roundtrip() {
    for p in [
      "anthropic",
      "openai",
      "groq",
      "xai",
      "google",
      "openrouter",
      "opencode",
      "custom",
    ] {
      assert_eq!(p.parse::<AiProvider>().unwrap().as_str(), p);
    }
    assert!("unknown".parse::<AiProvider>().is_err());
  }

  #[test]
  fn normalize_endpoint_accepts_and_rejects() {
    assert_eq!(
      normalize_endpoint("https://example.com/v1/").unwrap(),
      "https://example.com/v1"
    );
    assert_eq!(
      normalize_endpoint(" http://localhost:4096/v1 ").unwrap(),
      "http://localhost:4096/v1"
    );
    assert!(normalize_endpoint("ftp://example.com/v1").is_err());
    assert!(normalize_endpoint("not a url").is_err());
    assert!(normalize_endpoint("https://").is_err());
    assert!(normalize_endpoint("").is_err());
  }

  #[test]
  fn custom_requires_endpoint_and_opencode_defaults() {
    let tmp = tempfile::tempdir().unwrap();
    let _guard = crate::app_dirs::set_test_data_dir(tmp.path().to_path_buf());

    assert!(save_key("custom", "c1", "m", "k", None).is_err());
    let custom = save_key("custom", "c1", "m", "k", Some("http://localhost:11434/v1/")).unwrap();
    assert_eq!(
      custom.endpoint.as_deref(),
      Some("http://localhost:11434/v1")
    );

    let oc = save_key("opencode", "oc", "m", "k", None).unwrap();
    assert!(oc.endpoint.is_none());
    assert_eq!(
      AiProvider::Opencode.default_endpoint(),
      Some("https://opencode.ai/zen/go/v1/chat/completions")
    );
    assert!(AiProvider::Custom.is_openai_compatible());
    assert!(!AiProvider::Anthropic.is_openai_compatible());
  }

  #[test]
  fn compat_models_url_mapping() {
    assert_eq!(
      compat_models_url(
        Some("https://opencode.ai/zen/go/v1/chat/completions"),
        "https://opencode.ai/zen/go/v1/chat/completions"
      ),
      "https://opencode.ai/zen/go/v1/models"
    );
    assert_eq!(
      compat_models_url(Some("https://gw.example.com/v1"), "x"),
      "https://gw.example.com/v1/models"
    );
  }

  #[test]
  fn model_catalog_helpers_filter_and_shape() {
    // OpenAI-compatible shape: data[].id.
    let body = serde_json::json!({
      "object": "list",
      "data": [
        { "id": "gpt-4o-mini" },
        { "id": "gpt-4o-mini" },
        { "id": "whisper-large-v3" },
        { "id": "text-embedding-3-small" },
        { "id": 42 },
        { "other": "no-id" },
      ]
    });
    let items = body["data"].as_array().unwrap();
    let ids = extract_ids(items, "id", None);
    assert_eq!(
      finalize_models(ids),
      vec!["gpt-4o-mini".to_string()],
      "dedupes and drops non-chat ids"
    );

    // Google shape: models[].name with a `models/` prefix.
    let gbody = serde_json::json!({
      "models": [
        { "name": "models/gemini-2.5-flash" },
        { "name": "models/gemini-2.5-pro" },
      ]
    });
    let gitems = gbody["models"].as_array().unwrap();
    assert_eq!(
      finalize_models(extract_ids(gitems, "name", Some("models/"))),
      vec!["gemini-2.5-flash".to_string(), "gemini-2.5-pro".to_string()]
    );

    // Cap and blank handling.
    let many: Vec<String> = (0..600).map(|i| format!("model-{i}")).collect();
    assert_eq!(finalize_models(many).len(), MAX_MODEL_SUGGESTIONS);
    assert!(finalize_models(vec!["  ".to_string(), String::new()]).is_empty());
  }

  #[test]
  fn opencode_key_defaults_empty_name_to_opencode_go() {
    let tmp = tempfile::tempdir().unwrap();
    let _guard = crate::app_dirs::set_test_data_dir(tmp.path().to_path_buf());

    let oc = save_key("opencode", "", "kimi-k3", "k", None).unwrap();
    assert_eq!(oc.name, "OpenCode Go");
    // Whitespace-only names resolve the same way and update the same record.
    let oc2 = save_key("opencode", "   ", "kimi-k3", "k2", None).unwrap();
    assert_eq!(oc2.name, "OpenCode Go");
    assert_eq!(oc2.id, oc.id);

    // Every other provider still requires an explicit name.
    let err = save_key("openai", "", "gpt-4o-mini", "sk-x", None).unwrap_err();
    assert!(err.contains("NAME_CANNOT_BE_EMPTY"));
    let err = save_key("custom", "  ", "m", "k", Some("http://localhost:11434/v1")).unwrap_err();
    assert!(err.contains("NAME_CANNOT_BE_EMPTY"));
  }

  #[test]
  fn ai_keys_models_rejects_invalid_input() {
    let tmp = tempfile::tempdir().unwrap();
    let _guard = crate::app_dirs::set_test_data_dir(tmp.path().to_path_buf());
    let runtime = tokio::runtime::Runtime::new().unwrap();

    assert!(runtime
      .block_on(ai_keys_models("watson".to_string(), None, None, None))
      .is_err());
    assert!(runtime
      .block_on(ai_keys_models(
        "openai".to_string(),
        None,
        Some("missing-key-id".to_string()),
        None
      ))
      .is_err());
    assert!(runtime
      .block_on(ai_keys_models(
        "custom".to_string(),
        Some("ollama".to_string()),
        None,
        Some("ftp://example.com/v1".to_string())
      ))
      .is_err());
  }

  #[test]
  fn fetch_models_reads_catalog_and_degrades_gracefully() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
      let mock_server = wiremock::MockServer::start().await;
      // Only the authenticated request sees the catalog.
      wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path("/v1/models"))
        .and(wiremock::matchers::header("authorization", "Bearer k"))
        .respond_with(
          wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "object": "list",
            "data": [
              { "id": "kimi-k3" },
              { "id": "whisper-large-v3" },
              { "id": "kimi-k3" },
            ]
          })),
        )
        .mount(&mock_server)
        .await;

      let base = format!("{}/v1", mock_server.uri());
      let models = fetch_models(AiProvider::Opencode, Some("k"), Some(&base))
        .await
        .unwrap();
      assert_eq!(models, vec!["kimi-k3".to_string()]);

      // Wrong key: the mock 404s, which degrades to an empty list.
      let models = fetch_models(AiProvider::Opencode, Some("wrong"), Some(&base))
        .await
        .unwrap();
      assert!(models.is_empty());

      // Unreachable endpoint degrades instead of erroring.
      let models = fetch_models(AiProvider::Openai, Some("k"), Some("http://127.0.0.1:9/v1"))
        .await
        .unwrap();
      assert!(models.is_empty());

      // Providers that need a key (or custom without an endpoint) resolve
      // to empty without touching the network.
      assert!(fetch_models(AiProvider::Anthropic, None, None)
        .await
        .unwrap()
        .is_empty());
      assert!(fetch_models(AiProvider::Google, Some(""), None)
        .await
        .unwrap()
        .is_empty());
      assert!(fetch_models(AiProvider::Custom, Some("k"), None)
        .await
        .unwrap()
        .is_empty());
    });
  }

  #[test]
  fn legacy_records_without_endpoint_load() {
    let tmp = tempfile::tempdir().unwrap();
    let _guard = crate::app_dirs::set_test_data_dir(tmp.path().to_path_buf());
    let saved = save_key("openai", "legacy", "gpt-4o-mini", "sk-test-123456", None).unwrap();
    let raw = get_key(&saved.id).unwrap().unwrap();
    // Simulate a pre-endpoint vault entry.
    let legacy_json = serde_json::json!({
      "id": raw.id, "provider": raw.provider, "name": raw.name,
      "model": raw.model, "key": raw.key, "createdAt": raw.created_at,
    });
    let parsed: AiKeyRecord = serde_json::from_value(legacy_json).unwrap();
    assert!(parsed.endpoint.is_none());
  }
}
