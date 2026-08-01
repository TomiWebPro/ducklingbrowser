use std::fs;
use std::path::PathBuf;
use std::sync::LazyLock;

use chrono::{DateTime, LocalResult, NaiveDate, NaiveTime, TimeDelta, TimeZone, Utc};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::macro_step::MacroStep;

pub static SCHEDULER_STORE: LazyLock<SchedulerStore> = LazyLock::new(|| SchedulerStore);

fn default_timezone() -> String {
  "UTC".to_string()
}

fn default_jitter_minutes() -> u32 {
  30
}

fn default_true() -> bool {
  true
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Schedule {
  /// Local wall-clock window start, "HH:MM".
  pub window_start: String,
  /// Local wall-clock window end, "HH:MM". Must be strictly after start.
  pub window_end: String,
  /// IANA timezone name (chrono-tz), e.g. "America/New_York".
  #[serde(default = "default_timezone")]
  pub timezone: String,
  /// Random offset in minutes added around the daily target time.
  #[serde(default = "default_jitter_minutes")]
  pub jitter_minutes: u32,
  /// Re-roll the target time inside the window every day instead of sticking
  /// to one fixed time (anti-detection).
  #[serde(default = "default_true")]
  pub randomize_daily: bool,
}

impl Default for Schedule {
  fn default() -> Self {
    Self {
      window_start: "09:00".to_string(),
      window_end: "12:00".to_string(),
      timezone: default_timezone(),
      jitter_minutes: default_jitter_minutes(),
      randomize_daily: true,
    }
  }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TaskDefinition {
  pub id: String,
  pub name: String,
  #[serde(default)]
  pub description: Option<String>,
  /// "macro" executes compiled MacroSteps; "live_agent" delegates to an
  /// installed agent CLI.
  #[serde(default = "default_mode")]
  pub mode: String,
  /// Target profile for macro mode.
  #[serde(default)]
  pub profile_id: Option<String>,
  /// Registry agent id for live_agent mode.
  #[serde(default)]
  pub agent_id: Option<String>,
  /// Instruction text for live_agent mode.
  #[serde(default)]
  pub prompt: Option<String>,
  #[serde(default)]
  pub steps: Vec<MacroStep>,
  #[serde(default)]
  pub schedule: Schedule,
  /// Share the same per-hour automation quota as manual/MCP automation.
  #[serde(default = "default_true")]
  pub same_bucket_rate_limit: bool,
  #[serde(default = "default_true")]
  pub enabled: bool,
  #[serde(default)]
  pub created_at: String,
  #[serde(default)]
  pub updated_at: String,
  /// RFC3339 instant of the next scheduled run (computed on save).
  #[serde(default)]
  pub next_run_at: Option<String>,
  #[serde(default)]
  pub last_run_at: Option<String>,
  #[serde(default)]
  pub last_run_status: Option<String>,
  #[serde(default)]
  pub last_run_error: Option<String>,
  #[serde(default)]
  pub last_run_duration_ms: Option<u64>,
}

fn default_mode() -> String {
  "macro".to_string()
}

pub fn now_iso() -> String {
  Utc::now().to_rfc3339()
}

pub fn parse_window_time(hhmm: &str) -> Result<NaiveTime, String> {
  let mut parts = hhmm.split(':');
  let hour: u32 = parts
    .next()
    .and_then(|p| p.parse().ok())
    .ok_or_else(|| format!("invalid time '{hhmm}', expected HH:MM"))?;
  let minute: u32 = parts
    .next()
    .and_then(|p| p.parse().ok())
    .ok_or_else(|| format!("invalid time '{hhmm}', expected HH:MM"))?;
  NaiveTime::from_hms_opt(hour, minute, 0)
    .ok_or_else(|| format!("invalid time '{hhmm}', expected HH:MM"))
}

/// Deterministic uniform fraction in [0, 1) derived from a seed string.
fn fraction(seed: &str) -> f64 {
  let hash = blake3::hash(seed.as_bytes());
  let value = u64::from_le_bytes(hash.as_bytes()[..8].try_into().unwrap());
  (value >> 11) as f64 / (1u64 << 53) as f64
}

/// Deterministic target instant (UTC) inside the window on `date`, stable for
/// a given (task_id, date) so reloads don't move it within the same day.
/// Jitter is applied around the target and clamped to the window.
pub fn daily_target_utc(
  task_id: &str,
  date: NaiveDate,
  schedule: &Schedule,
) -> Result<DateTime<Utc>, String> {
  let tz: Tz = schedule
    .timezone
    .parse()
    .map_err(|_| format!("unknown timezone: {}", schedule.timezone))?;
  let start = parse_window_time(&schedule.window_start)?;
  let end = parse_window_time(&schedule.window_end)?;
  if end <= start {
    return Err(format!(
      "window end ({}) must be after window start ({})",
      schedule.window_end, schedule.window_start
    ));
  }

  let span_secs = (end - start).num_seconds();
  let base_seconds = fraction(&format!("{task_id}|{date}")) * span_secs as f64;
  let jitter_seconds = if schedule.jitter_minutes > 0 {
    (fraction(&format!("{task_id}|{date}|jitter")) * 2.0 - 1.0)
      * (schedule.jitter_minutes as f64 * 60.0)
  } else {
    0.0
  };
  let seconds = (base_seconds + jitter_seconds).clamp(0.0, span_secs as f64 - 1.0) as i64;

  let local = date.and_time(start) + TimeDelta::seconds(seconds);
  match tz.from_local_datetime(&local) {
    LocalResult::Single(dt) => Ok(dt.with_timezone(&Utc)),
    LocalResult::Ambiguous(dt, _) => Ok(dt.with_timezone(&Utc)),
    LocalResult::None => Ok(Utc.from_utc_datetime(&local)),
  }
}

/// Next run instant for an enabled task: today's target if still in the
/// future, otherwise tomorrow's target. Returns None when disabled.
pub fn compute_next_run(
  task: &TaskDefinition,
  now: DateTime<Utc>,
) -> Result<Option<DateTime<Utc>>, String> {
  if !task.enabled {
    return Ok(None);
  }
  let tz: Tz = task
    .schedule
    .timezone
    .parse()
    .map_err(|_| format!("unknown timezone: {}", task.schedule.timezone))?;
  let today = now.with_timezone(&tz).naive_local().date();
  let today_target = daily_target_utc(&task.id, today, &task.schedule)?;
  if today_target > now {
    Ok(Some(today_target))
  } else {
    let tomorrow_target = daily_target_utc(&task.id, today + TimeDelta::days(1), &task.schedule)?;
    Ok(Some(tomorrow_target))
  }
}

fn code_error(code: &str, params: serde_json::Value) -> String {
  serde_json::json!({ "code": code, "params": params }).to_string()
}

fn task_not_found(id: &str) -> String {
  code_error("TASK_NOT_FOUND", serde_json::json!({ "id": id }))
}

#[tauri::command]
pub fn scheduler_list() -> Vec<TaskDefinition> {
  SchedulerStore::instance().list_tasks()
}

#[tauri::command]
pub fn scheduler_save(task: TaskDefinition) -> Result<TaskDefinition, String> {
  SchedulerStore::instance().save_task(&task)
}

#[tauri::command]
pub fn scheduler_delete(id: String) -> Result<(), String> {
  SchedulerStore::instance().delete_task(&id)
}

#[tauri::command]
pub fn scheduler_set_enabled(id: String, enabled: bool) -> Result<TaskDefinition, String> {
  SchedulerStore::instance().set_enabled(&id, enabled)
}

pub struct SchedulerStore;

impl SchedulerStore {
  pub fn instance() -> &'static SchedulerStore {
    &SCHEDULER_STORE
  }

  fn tasks_file(&self) -> PathBuf {
    crate::app_dirs::settings_dir().join("scheduler_tasks.json")
  }

  pub fn load(&self) -> Vec<TaskDefinition> {
    let file = self.tasks_file();
    if !file.exists() {
      return Vec::new();
    }
    let Ok(content) = fs::read_to_string(&file) else {
      return Vec::new();
    };
    serde_json::from_str::<SchedulerFile>(&content)
      .map(|parsed| parsed.tasks)
      .unwrap_or_default()
  }

  fn persist(&self, tasks: &[TaskDefinition]) -> Result<(), String> {
    let dir = crate::app_dirs::settings_dir();
    fs::create_dir_all(&dir).map_err(|e| format!("Failed to create settings dir: {e}"))?;
    let payload = SchedulerFile {
      version: 1,
      tasks: tasks.to_vec(),
    };
    let json = serde_json::to_string_pretty(&payload).map_err(|e| format!("Serialize: {e}"))?;
    fs::write(self.tasks_file(), json).map_err(|e| format!("Failed to write tasks: {e}"))
  }

  pub fn list_tasks(&self) -> Vec<TaskDefinition> {
    self.load()
  }

  pub fn get_task(&self, id: &str) -> Option<TaskDefinition> {
    self.load().into_iter().find(|t| t.id == id)
  }

  pub fn save_task(&self, task: &TaskDefinition) -> Result<TaskDefinition, String> {
    if task.name.trim().is_empty() {
      return Err(code_error("NAME_CANNOT_BE_EMPTY", serde_json::json!({})));
    }
    if !matches!(task.mode.as_str(), "macro" | "live_agent") {
      return Err(code_error(
        "TASK_INVALID_SCHEDULE",
        serde_json::json!({ "detail": format!("unknown mode '{}'", task.mode) }),
      ));
    }
    parse_window_time(&task.schedule.window_start)?;
    parse_window_time(&task.schedule.window_end)?;
    daily_target_utc(&task.id, Utc::now().date_naive(), &task.schedule)?;

    let mut saved = task.clone();
    if saved.id.is_empty() {
      saved.id = Uuid::new_v4().to_string();
      saved.created_at = now_iso();
    }
    saved.updated_at = now_iso();
    saved.next_run_at = compute_next_run(&saved, Utc::now())?.map(|d| d.to_rfc3339());

    let mut all = self.load();
    if let Some(existing) = all.iter_mut().find(|t| t.id == saved.id) {
      *existing = saved.clone();
    } else {
      all.push(saved.clone());
    }
    self.persist(&all)?;
    Ok(saved)
  }

  pub fn delete_task(&self, id: &str) -> Result<(), String> {
    let mut all = self.load();
    let before = all.len();
    all.retain(|t| t.id != id);
    if all.len() == before {
      return Err(task_not_found(id));
    }
    self.persist(&all)
  }

  pub fn set_enabled(&self, id: &str, enabled: bool) -> Result<TaskDefinition, String> {
    let task = self.get_task(id).ok_or_else(|| task_not_found(id))?;
    let mut updated = task;
    updated.enabled = enabled;
    self.save_task(&updated)
  }
}

#[derive(Serialize, Deserialize)]
struct SchedulerFile {
  version: u32,
  tasks: Vec<TaskDefinition>,
}

#[cfg(test)]
mod tests {
  use super::*;
  use chrono::Timelike;

  fn test_schedule(hhmm_start: &str, hhmm_end: &str, timezone: &str) -> Schedule {
    Schedule {
      window_start: hhmm_start.to_string(),
      window_end: hhmm_end.to_string(),
      timezone: timezone.to_string(),
      jitter_minutes: 30,
      randomize_daily: true,
    }
  }

  fn task(id: &str, schedule: Schedule) -> TaskDefinition {
    TaskDefinition {
      id: id.to_string(),
      name: "Test task".to_string(),
      description: None,
      mode: "macro".to_string(),
      profile_id: None,
      agent_id: None,
      prompt: None,
      steps: vec![],
      schedule,
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

  #[test]
  fn parse_window_time_accepts_valid_and_rejects_garbage() {
    assert_eq!(
      parse_window_time("09:30").unwrap(),
      NaiveTime::from_hms_opt(9, 30, 0).unwrap()
    );
    assert!(parse_window_time("25:00").is_err());
    assert!(parse_window_time("09").is_err());
    assert!(parse_window_time("09:xx").is_err());
  }

  #[test]
  fn daily_target_is_deterministic_and_inside_window() {
    let sched = test_schedule("08:00", "20:00", "UTC");
    let date = NaiveDate::from_ymd_opt(2026, 8, 2).unwrap();
    let first = daily_target_utc("task-1", date, &sched).unwrap();
    let second = daily_target_utc("task-1", date, &sched).unwrap();
    assert_eq!(first, second);
    let start = parse_window_time("08:00").unwrap();
    let end = parse_window_time("20:00").unwrap();
    let local = first.with_timezone(&chrono_tz::UTC).time();
    assert!(
      local >= start && local <= end,
      "target {local} out of window"
    );
  }

  #[test]
  fn daily_target_differs_across_days() {
    let sched = test_schedule("00:00", "23:59", "UTC");
    let day1 = NaiveDate::from_ymd_opt(2026, 8, 2).unwrap();
    let day2 = NaiveDate::from_ymd_opt(2026, 8, 3).unwrap();
    assert_ne!(
      daily_target_utc("task-1", day1, &sched).unwrap(),
      daily_target_utc("task-1", day2, &sched).unwrap()
    );
  }

  #[test]
  fn daily_target_respects_timezone() {
    let sched = test_schedule("09:00", "10:00", "America/New_York");
    let date = NaiveDate::from_ymd_opt(2026, 8, 2).unwrap();
    let target = daily_target_utc("task-tz", date, &sched).unwrap();
    let in_ny = target.with_timezone(&chrono_tz::America::New_York);
    assert_eq!(in_ny.time().hour(), 9);
  }

  #[test]
  fn daily_target_rejects_bad_schedule() {
    let sched = test_schedule("10:00", "09:00", "UTC");
    let date = NaiveDate::from_ymd_opt(2026, 8, 2).unwrap();
    assert!(daily_target_utc("task-x", date, &sched).is_err());
    let bad_tz = test_schedule("09:00", "10:00", "Mars/Olympus");
    assert!(daily_target_utc("task-x", date, &bad_tz).is_err());
  }

  #[test]
  fn compute_next_run_picks_today_or_tomorrow() {
    let sched = test_schedule("09:00", "10:00", "UTC");
    let t = task("task-n", sched);
    let morning = Utc.with_ymd_and_hms(2026, 8, 2, 8, 0, 0).single().unwrap();
    let next = compute_next_run(&t, morning).unwrap().unwrap();
    assert_eq!(
      next.date_naive(),
      NaiveDate::from_ymd_opt(2026, 8, 2).unwrap()
    );
    assert!(next.time().hour() == 9);

    let afternoon = Utc.with_ymd_and_hms(2026, 8, 2, 15, 0, 0).single().unwrap();
    let next = compute_next_run(&t, afternoon).unwrap().unwrap();
    assert_eq!(
      next.date_naive(),
      NaiveDate::from_ymd_opt(2026, 8, 3).unwrap()
    );
  }

  #[test]
  fn compute_next_run_returns_none_when_disabled() {
    let mut t = task("task-d", test_schedule("09:00", "10:00", "UTC"));
    t.enabled = false;
    assert_eq!(compute_next_run(&t, Utc::now()).unwrap(), None);
  }

  #[test]
  fn store_roundtrip_persists_and_deletes() {
    let tmp = tempfile::tempdir().unwrap();
    let _guard = crate::app_dirs::set_test_data_dir(tmp.path().to_path_buf());
    let store = SchedulerStore;
    let file = crate::app_dirs::settings_dir().join("scheduler_tasks.json");
    assert!(store.list_tasks().is_empty());

    let mut t = task("", test_schedule("02:00", "04:00", "UTC"));
    t.name = String::new();
    assert!(store.save_task(&t).is_err());

    t.name = "Nightly refresh".to_string();
    let saved = store.save_task(&t).unwrap();
    assert!(!saved.id.is_empty());
    assert!(saved.next_run_at.is_some());
    assert!(file.exists());

    let reloaded = store.get_task(&saved.id).unwrap();
    assert_eq!(reloaded.name, "Nightly refresh");
    assert!(reloaded.next_run_at.is_some());

    let disabled = store.set_enabled(&saved.id, false).unwrap();
    assert!(!disabled.enabled);
    assert!(disabled.next_run_at.is_none());

    assert!(store.delete_task(&saved.id).is_ok());
    assert!(store.delete_task(&saved.id).is_err());
    assert!(store.list_tasks().is_empty());
  }
}
