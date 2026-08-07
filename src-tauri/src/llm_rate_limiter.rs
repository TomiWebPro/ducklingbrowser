use std::collections::VecDeque;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use crate::automation_rate_limiter::RateLimitOutcome;

const RATE_LIMIT_WINDOW: Duration = Duration::from_secs(60 * 60);

/// Rolling-window hourly budget for LLM completions. The limit comes from the
/// `llm_requests_per_hour` setting (0 = unlimited) — separate from the
/// browser-automation quota.
#[derive(Default)]
struct LlmRateLimiter {
  requests: VecDeque<Instant>,
}

impl LlmRateLimiter {
  fn check_at(&mut self, requests_per_hour: u64, now: Instant) -> RateLimitOutcome {
    if requests_per_hour == 0 {
      return RateLimitOutcome::Unlimited;
    }

    while self
      .requests
      .front()
      .is_some_and(|started| now.duration_since(*started) >= RATE_LIMIT_WINDOW)
    {
      self.requests.pop_front();
    }

    if self.requests.len() as u64 >= requests_per_hour {
      let retry_after_secs = self
        .requests
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

    self.requests.push_back(now);
    RateLimitOutcome::Allowed {
      remaining: requests_per_hour.saturating_sub(self.requests.len() as u64),
    }
  }
}

static LLM_RATE_LIMITER: LazyLock<Mutex<LlmRateLimiter>> =
  LazyLock::new(|| Mutex::new(LlmRateLimiter::default()));

/// Consumes one unit of the hourly LLM budget; the limit is read fresh from
/// settings on every call so a settings change applies immediately.
pub fn check_llm_rate_limit() -> RateLimitOutcome {
  let requests_per_hour = crate::settings_manager::SettingsManager::instance()
    .load_settings()
    .ok()
    .map(|settings| settings.llm_requests_per_hour)
    .unwrap_or(crate::llm::LLM_DEFAULT_REQUESTS_PER_HOUR);
  LLM_RATE_LIMITER
    .lock()
    .unwrap_or_else(|poisoned| poisoned.into_inner())
    .check_at(requests_per_hour, Instant::now())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn rolling_window_limits_and_recovers() {
    let mut limiter = LlmRateLimiter::default();
    let now = Instant::now();

    assert_eq!(
      limiter.check_at(2, now),
      RateLimitOutcome::Allowed { remaining: 1 }
    );
    assert_eq!(
      limiter.check_at(2, now + Duration::from_secs(1)),
      RateLimitOutcome::Allowed { remaining: 0 }
    );
    assert_eq!(
      limiter.check_at(2, now + Duration::from_secs(2)),
      RateLimitOutcome::Limited {
        retry_after_secs: 3598
      }
    );
    assert_eq!(
      limiter.check_at(2, now + RATE_LIMIT_WINDOW),
      RateLimitOutcome::Allowed { remaining: 0 }
    );
    assert_eq!(
      limiter.check_at(2, now + RATE_LIMIT_WINDOW + Duration::from_secs(1)),
      RateLimitOutcome::Allowed { remaining: 0 }
    );
    assert_eq!(
      limiter.check_at(2, now + RATE_LIMIT_WINDOW * 2 + Duration::from_secs(1)),
      RateLimitOutcome::Allowed { remaining: 1 }
    );
  }

  #[test]
  fn zero_limit_is_unlimited_and_does_not_consume_capacity() {
    let mut limiter = LlmRateLimiter::default();
    let now = Instant::now();

    assert_eq!(limiter.check_at(0, now), RateLimitOutcome::Unlimited);
    assert_eq!(
      limiter.check_at(1, now),
      RateLimitOutcome::Allowed { remaining: 0 }
    );
  }
}
