use std::future::Future;
use std::sync::{LazyLock, Mutex};
use std::time::Instant;

use serde::Serialize;

/// Default global cap on concurrent browser launches when no setting is
/// configured. Min 1.
pub const MAX_CONCURRENT_LAUNCHES: usize = 8;

/// Error code returned by `try_launch` when the global cap is reached.
pub const LAUNCH_CONCURRENCY_LIMIT: &str = "LAUNCH_CONCURRENCY_LIMIT";

/// Aggregate launch statistics, surfaced via REST `/v1/system/status`.
#[derive(Debug, Clone, Copy, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct LaunchSchedulerMetrics {
  /// The semaphore cap in effect (fixed at first use; settings changes apply
  /// on the next process start).
  pub max_concurrent_launches: usize,
  /// Launches currently in flight (permit held).
  pub in_flight: usize,
  /// Total launches that entered the queue.
  pub queued_total: u64,
  /// Launches that completed successfully.
  pub completed_total: u64,
  /// Launches that failed.
  pub failed_total: u64,
  /// Average time spent waiting for a permit, milliseconds.
  pub average_queue_ms: f64,
  /// Average time spent actually launching, milliseconds.
  pub average_launch_ms: f64,
}

#[derive(Default)]
struct LaunchStats {
  queued_total: u64,
  completed_total: u64,
  failed_total: u64,
  total_queue_ms: u128,
  total_launch_ms: u128,
}

/// Global launch concurrency manager. Every browser launch funnels through
/// `queue_launch`, so concurrent REST batch runs, MCP batch runs, UI launches
/// and synchronizer fan-out share one bounded launch pipeline instead of
/// stacking unboundedly.
pub struct LaunchScheduler {
  cap: usize,
  semaphore: std::sync::Arc<tokio::sync::Semaphore>,
  stats: Mutex<LaunchStats>,
}

/// The configured concurrency cap (min 1).
pub fn max_concurrent_launches() -> usize {
  crate::settings_manager::SettingsManager::instance()
    .load_settings()
    .ok()
    .map(|settings| settings.max_concurrent_launches.max(1))
    .unwrap_or(MAX_CONCURRENT_LAUNCHES)
}

static LAUNCH_SCHEDULER: LazyLock<LaunchScheduler> =
  LazyLock::new(|| LaunchScheduler::new(max_concurrent_launches()));

impl LaunchScheduler {
  pub fn new(cap: usize) -> Self {
    let cap = cap.max(1);
    Self {
      cap,
      semaphore: std::sync::Arc::new(tokio::sync::Semaphore::new(cap)),
      stats: Mutex::new(LaunchStats::default()),
    }
  }

  pub fn instance() -> &'static LaunchScheduler {
    &LAUNCH_SCHEDULER
  }

  /// Run a launch without waiting: acquires a permit immediately or returns
  /// `LAUNCH_CONCURRENCY_LIMIT`. Suitable for callers that prefer a fast
  /// rejection over queueing.
  ///
  /// Not used by a production path yet (every launch funnels through
  /// `queue_launch`); exposed + tested as the designed fast-fail API for
  /// callers that want immediate backpressure feedback.
  #[allow(dead_code)]
  pub async fn try_launch<T, F>(&self, launch: F) -> Result<T, String>
  where
    F: Future<Output = Result<T, String>>,
  {
    let permit = self
      .semaphore
      .clone()
      .try_acquire_owned()
      .map_err(|_| crate::backend_error(LAUNCH_CONCURRENCY_LIMIT))?;
    self.run_with_permit(permit, launch).await
  }

  /// Run a launch through the global queue: waits (FIFO) for a permit, then
  /// runs the future while holding it. Queue time and launch time are
  /// recorded per launch. Used by every real launch path.
  pub async fn queue_launch<T, F>(&self, launch: F) -> Result<T, String>
  where
    F: Future<Output = Result<T, String>>,
  {
    let queued_at = Instant::now();
    let _permit = self
      .semaphore
      .clone()
      .acquire_owned()
      .await
      .map_err(|_| crate::backend_error(LAUNCH_CONCURRENCY_LIMIT))?;
    let queue_ms = queued_at.elapsed().as_millis();
    let launch_started = Instant::now();
    let result = launch.await;
    let launch_ms = launch_started.elapsed().as_millis();

    let mut stats = self
      .stats
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner());
    stats.queued_total += 1;
    stats.total_queue_ms += queue_ms;
    stats.total_launch_ms += launch_ms;
    match &result {
      Ok(_) => stats.completed_total += 1,
      Err(_) => stats.failed_total += 1,
    }
    result
  }

  async fn run_with_permit<T, F>(
    &self,
    _permit: tokio::sync::OwnedSemaphorePermit,
    launch: F,
  ) -> Result<T, String>
  where
    F: Future<Output = Result<T, String>>,
  {
    let launch_started = Instant::now();
    let result = launch.await;
    let launch_ms = launch_started.elapsed().as_millis();

    let mut stats = self
      .stats
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner());
    stats.queued_total += 1;
    stats.total_launch_ms += launch_ms;
    match &result {
      Ok(_) => stats.completed_total += 1,
      Err(_) => stats.failed_total += 1,
    }
    result
  }

  pub fn metrics(&self) -> LaunchSchedulerMetrics {
    let stats = self
      .stats
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner());
    let average_queue_ms = if stats.queued_total == 0 {
      0.0
    } else {
      stats.total_queue_ms as f64 / stats.queued_total as f64
    };
    let average_launch_ms = if stats.queued_total == 0 {
      0.0
    } else {
      stats.total_launch_ms as f64 / stats.queued_total as f64
    };
    LaunchSchedulerMetrics {
      max_concurrent_launches: self.cap,
      in_flight: self.cap.saturating_sub(self.semaphore.available_permits()),
      queued_total: stats.queued_total,
      completed_total: stats.completed_total,
      failed_total: stats.failed_total,
      average_queue_ms,
      average_launch_ms,
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::sync::atomic::{AtomicBool, Ordering};

  fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Runtime::new().unwrap()
  }

  #[test]
  fn try_launch_rejects_when_cap_reached_and_recovers() {
    let scheduler = LaunchScheduler::new(1);
    let runtime = runtime();

    runtime.block_on(async {
      let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
      let holder = scheduler.try_launch(async move {
        let _ = release_rx.await;
        Ok::<(), String>(())
      });
      tokio::pin!(holder);
      // One poll drives the launch to the release gate, holding the permit.
      let waker = std::task::Waker::noop();
      let mut cx = std::task::Context::from_waker(waker);
      assert!(
        holder.as_mut().poll(&mut cx).is_pending(),
        "holder must block on the release gate"
      );

      // Second launch must be rejected immediately while the cap is full.
      let rejected = scheduler.try_launch(async { Ok::<(), String>(()) }).await;
      assert_eq!(
        rejected.unwrap_err(),
        crate::backend_error(LAUNCH_CONCURRENCY_LIMIT)
      );

      // Releasing frees the slot for the next launch.
      let _ = release_tx.send(());
      holder.await.unwrap();
      scheduler
        .try_launch(async { Ok::<(), String>(()) })
        .await
        .unwrap();

      let metrics = scheduler.metrics();
      assert_eq!(metrics.max_concurrent_launches, 1);
      assert_eq!(metrics.in_flight, 0);
      assert_eq!(metrics.completed_total, 2);
      assert_eq!(metrics.failed_total, 0);
    });
  }

  #[test]
  fn queue_launch_waits_for_permit_and_records_metrics() {
    let scheduler = LaunchScheduler::new(1);
    let runtime = runtime();

    runtime.block_on(async {
      let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
      let holder = scheduler.queue_launch(async move {
        let _ = release_rx.await;
        Ok::<(), String>(())
      });
      tokio::pin!(holder);
      // One poll drives the launch to the release gate, holding the permit.
      let waker = std::task::Waker::noop();
      let mut cx = std::task::Context::from_waker(waker);
      assert!(
        holder.as_mut().poll(&mut cx).is_pending(),
        "holder must block on the release gate"
      );

      // The queued launch must not start while the cap is full.
      let started = std::sync::Arc::new(AtomicBool::new(false));
      let started_clone = started.clone();
      let queued = scheduler.queue_launch(async move {
        started_clone.store(true, Ordering::SeqCst);
        Ok::<(), String>(())
      });
      tokio::pin!(queued);
      assert!(
        queued.as_mut().poll(&mut cx).is_pending(),
        "queued launch must wait for the permit"
      );
      assert!(
        !started.load(Ordering::SeqCst),
        "queued launch must not run while the cap is full"
      );

      // Releasing the permit lets the queued launch run.
      tokio::time::sleep(std::time::Duration::from_millis(10)).await;
      let _ = release_tx.send(());
      holder.await.unwrap();
      queued.await.unwrap();
      assert!(
        started.load(Ordering::SeqCst),
        "queued launch ran after release"
      );

      let metrics = scheduler.metrics();
      assert_eq!(metrics.queued_total, 2);
      assert_eq!(metrics.completed_total, 2);
      assert!(
        metrics.average_queue_ms > 0.0,
        "queue time must be recorded"
      );
      assert!(metrics.average_launch_ms >= 0.0);
    });
  }

  #[test]
  fn failures_are_counted_separately() {
    let scheduler = LaunchScheduler::new(2);
    let runtime = runtime();

    runtime.block_on(async {
      scheduler
        .queue_launch(async { Err::<(), String>("boom".to_string()) })
        .await
        .unwrap_err();
      let metrics = scheduler.metrics();
      assert_eq!(metrics.completed_total, 0);
      assert_eq!(metrics.failed_total, 1);
      assert_eq!(metrics.in_flight, 0);
    });
  }

  #[test]
  fn concurrency_cap_is_never_zero() {
    let scheduler = LaunchScheduler::new(0);
    assert_eq!(scheduler.metrics().max_concurrent_launches, 1);
  }
}
