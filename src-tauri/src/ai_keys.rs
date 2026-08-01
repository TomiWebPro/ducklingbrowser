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
  Google,
  Openrouter,
}

impl AiProvider {
  pub fn as_str(&self) -> &'static str {
    match self {
      AiProvider::Anthropic => "anthropic",
      AiProvider::Openai => "openai",
      AiProvider::Groq => "groq",
      AiProvider::Google => "google",
      AiProvider::Openrouter => "openrouter",
    }
  }
}

impl std::str::FromStr for AiProvider {
  type Err = ();

  fn from_str(s: &str) -> Result<Self, Self::Err> {
    match s {
      "anthropic" => Ok(AiProvider::Anthropic),
      "openai" => Ok(AiProvider::Openai),
      "groq" => Ok(AiProvider::Groq),
      "google" => Ok(AiProvider::Google),
      "openrouter" => Ok(AiProvider::Openrouter),
      _ => Err(()),
    }
  }
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

pub fn save_key(provider: &str, name: &str, model: &str, key: &str) -> Result<AiKeyInfo, String> {
  if name.trim().is_empty() {
    return Err(crate::backend_error("NAME_CANNOT_BE_EMPTY"));
  }
  if model.trim().is_empty() {
    return Err(invalid("model must not be empty"));
  }
  if key.trim().is_empty() {
    return Err(invalid("key must not be empty"));
  }
  let provider_parsed: AiProvider = provider
    .parse()
    .map_err(|_| invalid(&format!("unknown provider '{provider}'")))?;

  let mut records = load_records();
  let now = now_iso();
  let record = if let Some(existing) = records.iter_mut().find(|r| r.name == name.trim()) {
    existing.provider = provider_parsed.as_str().to_string();
    existing.model = model.trim().to_string();
    existing.key = key.trim().to_string();
    existing.clone()
  } else {
    let record = AiKeyRecord {
      id: Uuid::new_v4().to_string(),
      provider: provider_parsed.as_str().to_string(),
      name: name.trim().to_string(),
      model: model.trim().to_string(),
      key: key.trim().to_string(),
      created_at: now,
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

async fn probe(provider: AiProvider, key: &str, model: &str) -> Result<serde_json::Value, String> {
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
      let response = client
        .get("https://api.openai.com/v1/models")
        .bearer_auth(key)
        .send()
        .await;
      probe_outcome(response)
    }
    AiProvider::Groq => {
      let response = client
        .get("https://api.groq.com/openai/v1/models")
        .bearer_auth(key)
        .send()
        .await;
      probe_outcome(response)
    }
    AiProvider::Openrouter => {
      let response = client
        .get("https://openrouter.ai/api/v1/models")
        .bearer_auth(key)
        .send()
        .await;
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
) -> Result<AiKeyInfo, String> {
  save_key(&provider, &name, &model, &key)
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
) -> Result<serde_json::Value, String> {
  let provider_parsed: AiProvider = provider
    .parse()
    .map_err(|_| invalid(&format!("unknown provider '{provider}'")))?;
  let plaintext = match key {
    Some(k) if !k.trim().is_empty() => k.trim().to_string(),
    _ => {
      let id = id.ok_or_else(|| invalid("provide a key or a saved key id"))?;
      let record = get_key(&id)?.ok_or_else(|| not_found(&id))?;
      record.key
    }
  };
  probe(provider_parsed, &plaintext, &model).await
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

    let saved = save_key("openai", "Main key", "gpt-4o-mini", "sk-test-123456").unwrap();
    assert_eq!(saved.masked_key, "sk-***3456");
    assert!(get_key(&saved.id).unwrap().is_some());

    let listed = list_keys().unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].provider, "openai");

    assert!(save_key("bad-provider", "x", "model", "key").is_err());
    assert!(save_key("openai", "", "model", "key").is_err());

    delete_key(&saved.id).unwrap();
    assert!(list_keys().unwrap().is_empty());
    assert!(delete_key(&saved.id).is_err());
  }

  #[test]
  fn provider_roundtrip() {
    for p in ["anthropic", "openai", "groq", "google", "openrouter"] {
      assert_eq!(p.parse::<AiProvider>().unwrap().as_str(), p);
    }
    assert!("unknown".parse::<AiProvider>().is_err());
  }
}
