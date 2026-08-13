use std::collections::{HashMap, VecDeque};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use crate::cloud_auth::CLOUD_AUTH;

const RATE_LIMIT_WINDOW: Duration = Duration::from_secs(60 * 60);

/// Fallback automation quota (per hour) when neither a per-identity override
/// nor a configured setting exists. Mirrors the backend's default.
pub const DEFAULT_REQUESTS_PER_HOUR: u64 = 100;

/// The identity the limiter uses when no backend override is in play.
pub const DEFAULT_IDENTITY: &str = "internal";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateLimitOutcome {
  Unlimited,
  Allowed { remaining: u64 },
  Limited { retry_after_secs: u64 },
}

impl RateLimitOutcome {
  /// Whether the call was rejected by the quota.
  pub fn is_limited(&self) -> bool {
    matches!(self, RateLimitOutcome::Limited { .. })
  }
}

#[derive(Default)]
struct AutomationRateLimiter {
  requests: HashMap<String, VecDeque<Instant>>,
}

impl AutomationRateLimiter {
  fn check_at(&mut self, identity: &str, requests_per_hour: u64, now: Instant) -> RateLimitOutcome {
    if requests_per_hour == 0 {
      return RateLimitOutcome::Unlimited;
    }

    self.requests.retain(|_, requests| {
      while requests
        .front()
        .is_some_and(|started| now.duration_since(*started) >= RATE_LIMIT_WINDOW)
      {
        requests.pop_front();
      }
      !requests.is_empty()
    });

    let requests = self.requests.entry(identity.to_string()).or_default();
    if requests.len() as u64 >= requests_per_hour {
      let retry_after_secs = requests
        .front()
        .map(|started| {
          let remaining = RATE_LIMIT_WINDOW.saturating_sub(now.duration_since(*started));
          remaining
            .as_secs()
            .saturating_add(u64::from(remaining.subsec_nanos() > 0))
            .max(1)
        })
        .unwrap_or(1);
      return RateLimitOutcome::Limited { retry_after_secs };
    }

    requests.push_back(now);
    RateLimitOutcome::Allowed {
      remaining: requests_per_hour.saturating_sub(requests.len() as u64),
    }
  }
}

static AUTOMATION_RATE_LIMITER: LazyLock<Mutex<AutomationRateLimiter>> =
  LazyLock::new(|| Mutex::new(AutomationRateLimiter::default()));

pub async fn check_automation_rate_limit() -> RateLimitOutcome {
  // The local `automation_requests_per_hour` setting (0 = unlimited) is the
  // default. In e2e builds the harness can seed it via the environment so a
  // suite can prove the 429/Retry-After path with a tiny quota.
  let requests_per_hour_setting = crate::settings_manager::SettingsManager::instance()
    .load_settings()
    .ok()
    .map(|settings| settings.automation_requests_per_hour)
    .unwrap_or(DEFAULT_REQUESTS_PER_HOUR);
  let settings = e2e_requests_per_hour_override().unwrap_or(requests_per_hour_setting);

  let (identity, requests_per_hour) = match CLOUD_AUTH.automation_rate_limit().await {
    Some((identity, limit)) if identity != DEFAULT_IDENTITY && limit > 0 => (identity, limit),
    _ => (DEFAULT_IDENTITY.to_string(), settings),
  };

  AUTOMATION_RATE_LIMITER
    .lock()
    .unwrap_or_else(|poisoned| poisoned.into_inner())
    .check_at(&identity, requests_per_hour, Instant::now())
}

/// E2E-only quota seeding. The integrations suite sets
/// `DUCKLING_E2E_REQUESTS_PER_HOUR` to prove the 429 path; normal builds have
/// no override and use the settings value.
fn e2e_requests_per_hour_override() -> Option<u64> {
  #[cfg(feature = "e2e")]
  {
    std::env::var("DUCKLING_E2E_REQUESTS_PER_HOUR")
      .ok()
      .and_then(|value| value.parse::<u64>().ok())
  }
  #[cfg(not(feature = "e2e"))]
  {
    None
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn rolling_window_limits_per_identity_and_recovers() {
    let mut limiter = AutomationRateLimiter::default();
    let now = Instant::now();

    assert_eq!(
      limiter.check_at("user-a", 2, now),
      RateLimitOutcome::Allowed { remaining: 1 }
    );
    assert_eq!(
      limiter.check_at("user-a", 2, now + Duration::from_secs(1)),
      RateLimitOutcome::Allowed { remaining: 0 }
    );
    assert_eq!(
      limiter.check_at("user-a", 2, now + Duration::from_secs(2)),
      RateLimitOutcome::Limited {
        retry_after_secs: 3598
      }
    );

    assert_eq!(
      limiter.check_at("user-b", 2, now + Duration::from_secs(2)),
      RateLimitOutcome::Allowed { remaining: 1 }
    );
    assert_eq!(
      limiter.check_at("user-a", 2, now + RATE_LIMIT_WINDOW),
      RateLimitOutcome::Allowed { remaining: 0 }
    );
    assert_eq!(
      limiter.check_at(
        "user-a",
        2,
        now + RATE_LIMIT_WINDOW + Duration::from_secs(1)
      ),
      RateLimitOutcome::Allowed { remaining: 0 }
    );
    assert_eq!(
      limiter.check_at(
        "user-a",
        2,
        now + RATE_LIMIT_WINDOW * 2 + Duration::from_secs(1)
      ),
      RateLimitOutcome::Allowed { remaining: 1 }
    );
  }

  #[test]
  fn zero_limit_is_unlimited_and_does_not_consume_capacity() {
    let mut limiter = AutomationRateLimiter::default();
    let now = Instant::now();

    assert_eq!(
      limiter.check_at("user-a", 0, now),
      RateLimitOutcome::Unlimited
    );
    assert_eq!(
      limiter.check_at("user-a", 1, now),
      RateLimitOutcome::Allowed { remaining: 0 }
    );
  }

  #[test]
  #[serial_test::serial]
  fn settings_override_controls_the_shared_limit() {
    use crate::settings_manager::{AppSettings, SettingsManager};

    // Zero in settings = unlimited for the whole app (fleet mode).
    {
      let tmp = tempfile::tempdir().unwrap();
      let _guard = crate::app_dirs::set_test_data_dir(tmp.path().to_path_buf());
      let settings = AppSettings {
        automation_requests_per_hour: 0,
        ..Default::default()
      };
      SettingsManager::instance()
        .save_settings(&settings)
        .unwrap();

      let outcome = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(check_automation_rate_limit());
      assert_eq!(outcome, RateLimitOutcome::Unlimited);
    }

    // A small budget is enforced and then exhausted.
    {
      let tmp = tempfile::tempdir().unwrap();
      let _guard = crate::app_dirs::set_test_data_dir(tmp.path().to_path_buf());
      let settings = AppSettings {
        automation_requests_per_hour: 2,
        ..Default::default()
      };
      SettingsManager::instance()
        .save_settings(&settings)
        .unwrap();

      let mut outcome = RateLimitOutcome::Unlimited;
      for _ in 0..3 {
        outcome = tokio::runtime::Runtime::new()
          .unwrap()
          .block_on(check_automation_rate_limit());
      }
      assert!(
        matches!(outcome, RateLimitOutcome::Limited { retry_after_secs } if retry_after_secs >= 1)
      );
    }
  }
}
