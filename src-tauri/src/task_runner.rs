use std::collections::BTreeMap;

use serde_json::Value;

use crate::cdp_session::{CdpError, CdpSessionTrait};
use crate::macro_step::MacroStep;
use crate::profile::ProfileManager;
use crate::scheduler::TaskDefinition;

/// Per-step execution timeout.
const STEP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

#[derive(Debug, Clone)]
pub struct TaskRunResult {
  pub status: String,
  pub error: Option<String>,
  pub duration_ms: u64,
  pub extracted: BTreeMap<String, Value>,
  /// SaveProfileField steps that need the Tauri app context; the JobRunner
  /// applies them after the trait-based session finishes.
  pub deferred_profile_fields: Vec<(String, Value)>,
}

fn step_timeout_err(step: &MacroStep) -> String {
  format!("Step {step:?} timed out after {STEP_TIMEOUT:?}")
}

fn click_expression(selector: Option<&str>, index: Option<u32>) -> Result<String, String> {
  if let Some(index) = index {
    return Ok(format!(
      r#"(() => {{
        const arr = window.__duckling_interactive;
        if (!arr || !arr[{index}]) throw new Error('No element at index {index}');
        const el = arr[{index}];
        el.scrollIntoView({{block: 'center'}});
        el.click();
        return true;
      }})()"#
    ));
  }
  let selector = selector.ok_or_else(|| "Click requires a selector or an index".to_string())?;
  let escaped = selector.replace('\\', "\\\\").replace('\'', "\\'");
  Ok(format!(
    r#"(() => {{
      const el = document.querySelector('{escaped}');
      if (!el) throw new Error('Element not found: {escaped}');
      el.scrollIntoView({{block: 'center'}});
      el.click();
      return true;
    }})()"#
  ))
}

fn type_expression(
  selector: Option<&str>,
  index: Option<u32>,
  text: &str,
) -> Result<String, String> {
  let target = if let Some(index) = index {
    format!(
      r#"(window.__duckling_interactive && window.__duckling_interactive[{index}]) || (() => {{ throw new Error('No element at index {index}'); }})()"#
    )
  } else {
    let selector = selector.ok_or_else(|| "Type requires a selector or an index".to_string())?;
    let escaped = selector.replace('\\', "\\\\").replace('\'', "\\'");
    format!(
      r#"document.querySelector('{escaped}') || (() => {{ throw new Error('Element not found: {escaped}'); }})()"#
    )
  };
  let json_text = serde_json::to_string(text).unwrap_or_else(|_| "\"\"".to_string());
  Ok(format!(
    r#"(() => {{
      const el = {target};
      el.focus();
      const proto = el instanceof HTMLTextAreaElement ? HTMLTextAreaElement.prototype : HTMLInputElement.prototype;
      const setter = Object.getOwnPropertyDescriptor(proto, 'value').set;
      setter.call(el, {json_text});
      el.dispatchEvent(new Event('input', {{ bubbles: true }}));
      el.dispatchEvent(new Event('change', {{ bubbles: true }}));
      return true;
    }})()"#
  ))
}

fn save_profile_field(path: &str, value: &Value) -> Result<(), String> {
  let rest = path
    .strip_prefix("profile.")
    .ok_or_else(|| format!("Unsupported profile field path '{path}'"))?;
  let (profile_id, field) = rest
    .split_once('.')
    .ok_or_else(|| format!("Unsupported profile field path '{path}'"))?;

  let manager = ProfileManager::instance();
  let profiles = manager.list_profiles().map_err(|e| e.to_string())?;
  let mut profile = profiles
    .into_iter()
    .find(|p| p.id.to_string() == profile_id)
    .ok_or_else(|| format!("Profile '{profile_id}' not found"))?;

  match field {
    "note" => {
      profile.note = value
        .as_str()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    }
    "window_color" => {
      profile.window_color = value.as_str().map(|s| s.to_string());
    }
    "clear_on_close" => {
      let flag = value
        .as_bool()
        .ok_or_else(|| "clear_on_close must be a boolean".to_string())?;
      profile.clear_on_close = flag;
    }
    other => return Err(format!("Unsupported profile field '{other}'")),
  }
  profile.updated_at = Some(crate::proxy_manager::now_secs());
  manager.save_profile(&profile).map_err(|e| e.to_string())
}

/// Resolve the running profile's WebSocket URL, or None when the task has no
/// profile or the profile is not running (macro steps that need the page will
/// then fail with a clear message).
async fn resolve_ws_url(
  task: &TaskDefinition,
  session: &mut impl CdpSessionTrait,
) -> Result<String, String> {
  let profile_id = task
    .profile_id
    .as_deref()
    .ok_or_else(|| "Task has no target profile".to_string())?;
  let profiles = ProfileManager::instance()
    .list_profiles()
    .map_err(|e| format!("Failed to list profiles: {e}"))?;
  let profile = profiles
    .into_iter()
    .find(|p| p.id.to_string() == profile_id)
    .ok_or_else(|| format!("Profile '{profile_id}' not found"))?;
  session
    .resolve_ws_url(&profile)
    .await
    .map_err(|e| format!("Profile is not running: {}", e.message))
}

pub async fn run_task(
  task: &TaskDefinition,
  session: &mut impl CdpSessionTrait,
) -> Result<TaskRunResult, String> {
  let started = std::time::Instant::now();
  match task.mode.as_str() {
    "macro" => run_macro(task, session).await,
    "live_agent" => {
      // live_agent runs through the agent engine; this executor only handles
      // macro mode. The JobRunner routes live_agent tasks to the delegation
      // path before calling run_task.
      Err("live_agent tasks must be routed through the agent engine".to_string())
    }
    other => Err(format!("Unknown task mode '{other}'")),
  }
  .map(|mut result| {
    result.duration_ms = started.elapsed().as_millis() as u64;
    result
  })
}

async fn run_macro(
  task: &TaskDefinition,
  session: &mut impl CdpSessionTrait,
) -> Result<TaskRunResult, String> {
  let ws_url = resolve_ws_url(task, session).await?;
  let mut extracted = BTreeMap::new();
  let mut deferred: Vec<(String, Value)> = Vec::new();

  for step in &task.steps {
    let outcome = tokio::time::timeout(
      STEP_TIMEOUT,
      execute_step(task, session, &ws_url, step, &mut extracted, &mut deferred),
    )
    .await
    .map_err(|_| step_timeout_err(step))??;
    if let Some(error) = outcome {
      return Ok(TaskRunResult {
        status: "error".to_string(),
        error: Some(error),
        duration_ms: 0,
        extracted,
        deferred_profile_fields: deferred,
      });
    }
  }

  Ok(TaskRunResult {
    status: "success".to_string(),
    error: None,
    duration_ms: 0,
    extracted,
    deferred_profile_fields: deferred,
  })
}

/// Executes one macro step. Returns Ok(Some(error)) when the step failed
/// (short-circuit the run), Ok(None) on success.
async fn execute_step(
  task: &TaskDefinition,
  session: &mut impl CdpSessionTrait,
  ws_url: &str,
  step: &MacroStep,
  extracted: &mut BTreeMap<String, Value>,
  deferred: &mut Vec<(String, Value)>,
) -> Result<Option<String>, String> {
  match step {
    MacroStep::Navigate { url } => {
      session
        .navigate(ws_url, url)
        .await
        .map_err(|e: CdpError| e.message)?;
      Ok(None)
    }
    MacroStep::WaitSelector {
      selector,
      timeout_ms,
    } => {
      session
        .wait_for_selector(ws_url, selector, timeout_ms.unwrap_or(30_000))
        .await
        .map_err(|e| e.message)?;
      Ok(None)
    }
    MacroStep::Click { selector, index } => {
      let expression = click_expression(selector.as_deref(), *index)?;
      session
        .evaluate(ws_url, &expression)
        .await
        .map_err(|e| e.message)?;
      Ok(None)
    }
    MacroStep::Type {
      selector,
      index,
      text,
    } => {
      let expression = type_expression(selector.as_deref(), *index, text)?;
      session
        .evaluate(ws_url, &expression)
        .await
        .map_err(|e| e.message)?;
      Ok(None)
    }
    MacroStep::Evaluate { expression } => {
      session
        .evaluate(ws_url, expression)
        .await
        .map_err(|e| e.message)?;
      Ok(None)
    }
    MacroStep::Screenshot => {
      session.screenshot(ws_url).await.map_err(|e| e.message)?;
      Ok(None)
    }
    MacroStep::Extract { expression, key } => {
      let result = session
        .evaluate(ws_url, expression)
        .await
        .map_err(|e| e.message)?;
      extracted.insert(key.clone(), result);
      Ok(None)
    }
    MacroStep::SaveProfileField { path, value } => {
      if task.profile_id.is_some() {
        deferred.push((path.clone(), value.clone()));
      }
      Ok(None)
    }
  }
}

/// Applies deferred profile-field saves; called by the JobRunner with the app
/// context available.
pub fn apply_deferred_profile_fields(fields: &[(String, Value)]) -> Result<(), String> {
  for (path, value) in fields {
    save_profile_field(path, value)?;
  }
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;
  use serde_json::json;

  struct FakeCdpSession {
    navigations: Vec<String>,
    evaluations: Vec<String>,
  }

  impl FakeCdpSession {
    fn new() -> Self {
      Self {
        navigations: Vec::new(),
        evaluations: Vec::new(),
      }
    }
  }

  #[async_trait::async_trait]
  impl CdpSessionTrait for FakeCdpSession {
    async fn resolve_ws_url(
      &mut self,
      _profile: &crate::profile::BrowserProfile,
    ) -> Result<String, CdpError> {
      Ok("ws://fake".to_string())
    }
    async fn navigate(&mut self, _ws_url: &str, url: &str) -> Result<Value, CdpError> {
      self.navigations.push(url.to_string());
      Ok(json!({ "ok": true }))
    }
    async fn evaluate(&mut self, _ws_url: &str, expression: &str) -> Result<Value, CdpError> {
      self.evaluations.push(expression.to_string());
      Ok(json!({ "value": 42 }))
    }
    async fn screenshot(&mut self, _ws_url: &str) -> Result<Value, CdpError> {
      Ok(json!({ "data": "base64" }))
    }
    async fn wait_for_selector(
      &mut self,
      _ws_url: &str,
      _selector: &str,
      _timeout_ms: u64,
    ) -> Result<(), CdpError> {
      Ok(())
    }
  }

  fn task(steps: Vec<MacroStep>) -> TaskDefinition {
    TaskDefinition {
      id: "task-1".to_string(),
      name: "Test task".to_string(),
      description: None,
      mode: "macro".to_string(),
      profile_id: Some("profile-1".to_string()),
      agent_id: None,
      prompt: None,
      steps,
      schedule: Default::default(),
      same_bucket_rate_limit: true,
      enabled: true,
      created_at: String::new(),
      updated_at: String::new(),
      next_run_at: None,
      last_run_at: None,
      last_run_status: None,
      last_run_error: None,
      last_run_duration_ms: None,
    }
  }

  async fn run_with_fake(task: &TaskDefinition) -> Result<TaskRunResult, String> {
    let mut fake = FakeCdpSession::new();
    let mut result = run_macro(task, &mut fake).await?;
    result.duration_ms = 1;
    let _ = fake;
    Ok(result)
  }

  #[test]
  fn click_expression_uses_selector() {
    let expr = click_expression(Some("button#go"), None).unwrap();
    assert!(expr.contains("querySelector('button#go')"));
    assert!(expr.contains("el.click()"));
    assert!(click_expression(None, None).is_err());
  }

  #[test]
  fn click_expression_escapes_quotes() {
    let expr = click_expression(Some("a[title='x']"), None).unwrap();
    assert!(expr.contains("querySelector('a[title=\\'x\\']')"));
  }

  #[test]
  fn click_expression_by_index() {
    let expr = click_expression(None, Some(3)).unwrap();
    assert!(expr.contains("arr[3]"));
  }

  #[test]
  fn type_expression_sets_value() {
    let expr = type_expression(Some("input#q"), None, "hello \"world\"").unwrap();
    assert!(expr.contains("querySelector('input#q')"));
    assert!(expr.contains("\"hello \\\"world\\\"\""));
  }

  #[test]
  fn type_expression_by_index() {
    let expr = type_expression(None, Some(2), "x").unwrap();
    assert!(expr.contains("__duckling_interactive[2]"));
  }

  #[test]
  fn save_profile_field_rejects_unknown_paths() {
    assert!(save_profile_field("other.path", &json!("hi")).is_err());
    assert!(save_profile_field("profile", &json!("hi")).is_err());
    let tmp = tempfile::tempdir().unwrap();
    let _guard = crate::app_dirs::set_test_data_dir(tmp.path().to_path_buf());
    assert!(save_profile_field("profile.note", &json!("hi")).is_err());
    assert!(save_profile_field("profile.clear_on_close", &json!("not-a-bool")).is_err());
  }

  #[tokio::test]
  async fn unknown_mode_fails() {
    let mut t = task(Vec::new());
    t.mode = "bogus".to_string();
    let result = run_with_fake(&t).await;
    assert!(result.is_err());
  }

  #[tokio::test]
  async fn live_agent_mode_requires_routing() {
    let mut t = task(Vec::new());
    t.mode = "live_agent".to_string();
    let result = run_with_fake(&t).await;
    assert!(result.is_err());
  }

  fn seeded_profile() -> String {
    let profile = crate::profile::BrowserProfile {
      id: uuid::Uuid::new_v4(),
      name: "Test profile".to_string(),
      browser: "chromium".to_string(),
      version: "130.0".to_string(),
      proxy_id: None,
      vpn_id: None,
      launch_hook: None,
      process_id: None,
      last_launch: None,
      release_type: "stable".to_string(),
      chromium_config: None,
      group_id: None,
      tags: Vec::new(),
      note: None,
      window_color: None,
      sync_mode: crate::profile::types::SyncMode::Disabled,
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
      created_at: Some(0),
      updated_at: Some(0),
    };
    ProfileManager::instance().save_profile(&profile).unwrap();
    profile.id.to_string()
  }

  #[tokio::test]
  async fn run_task_executes_full_plan() {
    let tmp = tempfile::tempdir().unwrap();
    let _guard = crate::app_dirs::set_test_data_dir(tmp.path().to_path_buf());
    let profile_id = seeded_profile();

    let steps = vec![
      MacroStep::Navigate {
        url: "https://example.com".to_string(),
      },
      MacroStep::WaitSelector {
        selector: "#login".to_string(),
        timeout_ms: None,
      },
      MacroStep::Click {
        selector: Some("button.submit".to_string()),
        index: None,
      },
      MacroStep::Type {
        selector: None,
        index: Some(0),
        text: "hello".to_string(),
      },
      MacroStep::Evaluate {
        expression: "document.title".to_string(),
      },
      MacroStep::Extract {
        expression: "location.href".to_string(),
        key: "url".to_string(),
      },
      MacroStep::Screenshot,
      MacroStep::SaveProfileField {
        path: format!("profile.{profile_id}.note"),
        value: json!("from-macro"),
      },
    ];
    let mut t = task(steps);
    t.profile_id = Some(profile_id.clone());

    let mut fake = FakeCdpSession::new();
    let result = run_task(&t, &mut fake).await.unwrap();

    assert_eq!(result.status, "success");
    assert_eq!(result.error, None);
    assert_eq!(fake.navigations, vec!["https://example.com".to_string()]);
    assert_eq!(fake.evaluations.len(), 4, "click, type, evaluate, extract");
    assert_eq!(result.extracted.get("url"), Some(&json!({ "value": 42 })));
    assert_eq!(result.deferred_profile_fields.len(), 1);
    assert_eq!(
      result.deferred_profile_fields[0],
      (format!("profile.{profile_id}.note"), json!("from-macro"))
    );
  }

  #[tokio::test]
  async fn run_task_stops_at_failed_step() {
    let tmp = tempfile::tempdir().unwrap();
    let _guard = crate::app_dirs::set_test_data_dir(tmp.path().to_path_buf());
    let profile_id = seeded_profile();

    let steps = vec![
      MacroStep::Navigate {
        url: "https://example.com".to_string(),
      },
      MacroStep::Click {
        selector: None,
        index: None,
      },
      MacroStep::Screenshot,
    ];
    let mut t = task(steps);
    t.profile_id = Some(profile_id);

    let mut fake = FakeCdpSession::new();
    let error = run_task(&t, &mut fake).await.unwrap_err();
    assert!(error.contains("Click requires a selector or an index"));
    assert_eq!(
      fake.evaluations.len(),
      0,
      "steps after the failure must not run"
    );
    assert_eq!(fake.navigations.len(), 1);
  }

  #[tokio::test]
  async fn run_task_requires_running_profile() {
    let tmp = tempfile::tempdir().unwrap();
    let _guard = crate::app_dirs::set_test_data_dir(tmp.path().to_path_buf());
    let mut t = task(vec![MacroStep::Navigate {
      url: "https://example.com".to_string(),
    }]);
    t.profile_id = Some("no-such-profile".to_string());

    let mut fake = FakeCdpSession::new();
    let error = run_task(&t, &mut fake).await.unwrap_err();
    assert!(error.contains("not found"));
  }

  #[test]
  fn save_profile_field_updates_whitelisted_fields() {
    let tmp = tempfile::tempdir().unwrap();
    let _guard = crate::app_dirs::set_test_data_dir(tmp.path().to_path_buf());
    let profile_id = seeded_profile();

    save_profile_field(&format!("profile.{profile_id}.note"), &json!("hello")).unwrap();
    save_profile_field(
      &format!("profile.{profile_id}.window_color"),
      &json!("#112233"),
    )
    .unwrap();
    save_profile_field(
      &format!("profile.{profile_id}.clear_on_close"),
      &json!(true),
    )
    .unwrap();
    assert!(
      save_profile_field(
        &format!("profile.{profile_id}.clear_on_close"),
        &json!("yes")
      )
      .is_err(),
      "clear_on_close must be a boolean"
    );
    assert!(
      save_profile_field(&format!("profile.{profile_id}.unknown_field"), &json!("x")).is_err()
    );

    let profiles = ProfileManager::instance().list_profiles().unwrap();
    let profile = profiles
      .iter()
      .find(|p| p.id.to_string() == profile_id)
      .expect("seeded profile must persist");
    assert_eq!(profile.note.as_deref(), Some("hello"));
    assert_eq!(profile.window_color.as_deref(), Some("#112233"));
    assert!(profile.clear_on_close);
  }
}
