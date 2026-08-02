//! Supervised Xray subprocess that serves VLESS traffic over a local SOCKS5
//! inbound.
//!
//! The duckling-proxy sidecar's `vpn-worker start` mode spawns this worker for
//! VLESS configs: it renders the Xray client JSON for the requested local
//! SOCKS5 port, ensures the pinned xray binary is installed (see
//! `xray_vendor`), spawns `xray run -c`, writes back `local_url` like the
//! WireGuard worker does, and supervises the child until it exits.

use std::process::Stdio;

use super::config::VpnError;
use super::xray_vendor;

const WORKER_POLL_INTERVAL_MS: u64 = 100;

pub struct XrayWorker {
  config: super::VlessConfig,
  port: u16,
}

impl XrayWorker {
  pub fn new(config: super::VlessConfig, port: u16) -> Self {
    Self { config, port }
  }

  /// Render the Xray client JSON and persist it next to the worker's config
  /// file. Returns the path to the client config.
  fn write_client_config(&self, config_id: &str) -> Result<std::path::PathBuf, VpnError> {
    let doc = super::vless_config_to_xray_client_json(&self.config, self.port)?;
    let path = std::env::temp_dir().join(format!("duckling_xray_{config_id}.json"));
    let content = serde_json::to_string_pretty(&doc)
      .map_err(|e| VpnError::Connection(format!("Failed to serialize Xray config: {e}")))?;
    std::fs::write(&path, content)
      .map_err(|e| VpnError::Connection(format!("Failed to write Xray config: {e}")))?;
    #[cfg(unix)]
    {
      crate::app_dirs::restrict_to_owner(&path);
    }
    Ok(path)
  }

  /// The SOCKS5 endpoint the worker serves, for local_url write-back.
  fn local_url(&self) -> String {
    format!("socks5://127.0.0.1:{}", self.port)
  }

  /// Write local_url back into the worker config so the parent's readiness
  /// check sees it. Mirrors the WireGuard worker's write-back logic.
  fn write_back_local_url(&self, config_id: &str, config_path: Option<&std::path::Path>) {
    let updated = match config_path {
      Some(path) => crate::vpn_worker_storage::get_vpn_worker_config_from_path(path)
        .or_else(|| crate::vpn_worker_storage::get_vpn_worker_config(config_id)),
      None => crate::vpn_worker_storage::get_vpn_worker_config(config_id),
    };
    if let Some(mut wc) = updated {
      wc.local_port = Some(self.port);
      wc.local_url = Some(self.local_url());
      let result = match config_path {
        Some(path) => crate::vpn_worker_storage::save_vpn_worker_config_to_path(&wc, path)
          .map_err(|e| e.to_string()),
        None => crate::vpn_worker_storage::save_vpn_worker_config(&wc).map_err(|e| e.to_string()),
      };
      if let Err(e) = result {
        log::error!(
          "[vpn-worker] Failed to write back local_url to config: {} (path={:?})",
          e,
          config_path
        );
      }
    } else {
      log::error!(
        "[vpn-worker] Could not load worker config for write-back (id={}, path={:?})",
        config_id,
        config_path
      );
    }
  }

  /// Spawn `xray run -c` and supervise it until the worker is terminated.
  pub async fn run(
    &self,
    config_id: String,
    config_path: Option<std::path::PathBuf>,
  ) -> Result<(), VpnError> {
    let xray_binary = xray_vendor::ensure_xray_binary()
      .await
      .map_err(|e| VpnError::Connection(format!("Failed to install Xray binary: {e}")))?;
    log::info!(
      "[vpn-worker] Using Xray binary at {}",
      xray_binary.display()
    );

    let client_config_path = self.write_client_config(&config_id)?;

    self.write_back_local_url(&config_id, config_path.as_deref());
    log::info!(
      "[vpn-worker] Xray SOCKS5 inbound on 127.0.0.1:{}",
      self.port
    );

    let log_path = std::env::temp_dir().join(format!("duckling-xray-{config_id}.log"));
    let log_file = std::fs::OpenOptions::new()
      .create(true)
      .append(true)
      .open(&log_path)
      .map_err(|e| {
        VpnError::Connection(format!(
          "Failed to open xray log {}: {e}",
          log_path.display()
        ))
      })?;

    let mut cmd = std::process::Command::new(&xray_binary);
    cmd.arg("run");
    cmd.arg("-c");
    cmd.arg(&client_config_path);
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::null());
    cmd.stderr(Stdio::from(log_file));

    // Install the SIGTERM handler before spawning so a termination arriving
    // right after spawn is still caught (otherwise the default handler kills
    // this worker and orphans the xray child).
    #[cfg(unix)]
    let mut terminate =
      tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok();

    #[cfg(unix)]
    {
      use std::os::unix::process::CommandExt;
      unsafe {
        cmd.pre_exec(|| {
          libc::setsid();
          Ok(())
        });
      }
    }

    let mut child = cmd
      .spawn()
      .map_err(|e| VpnError::Connection(format!("Failed to spawn xray: {e}")))?;

    log::info!(
      "[vpn-worker] xray started (pid={}), client config {}",
      child.id(),
      client_config_path.display()
    );

    // Supervise: poll the child; if it exits before the worker is killed the
    // tunnel is dead, so report the failure like the WireGuard worker does.
    // When the worker itself receives SIGTERM (from stop_vpn_worker), kill
    // the xray child so the tunnel is torn down with the worker.
    let result: Result<(), VpnError> = loop {
      #[cfg(unix)]
      {
        let signal_fired = match &mut terminate {
          Some(sig) => tokio::select! {
            _ = sig.recv() => true,
            _ = tokio::time::sleep(tokio::time::Duration::from_millis(
              WORKER_POLL_INTERVAL_MS,
            )) => false,
          },
          None => {
            tokio::time::sleep(tokio::time::Duration::from_millis(WORKER_POLL_INTERVAL_MS)).await;
            false
          }
        };
        if signal_fired {
          log::info!("[vpn-worker] Terminating xray child {}", child.id());
          let _ = child.kill();
          let _ = child.wait();
          break Ok(());
        }
      }
      #[cfg(not(unix))]
      {
        tokio::time::sleep(tokio::time::Duration::from_millis(WORKER_POLL_INTERVAL_MS)).await;
      }

      match child.try_wait() {
        Ok(Some(status)) => {
          break Err(VpnError::Connection(format!(
            "xray process exited unexpectedly: {status}. Log: {}",
            log_path.display()
          )));
        }
        Ok(None) => {}
        Err(e) => {
          break Err(VpnError::Connection(format!("Failed to wait on xray: {e}")));
        }
      }
    };

    // The client config carries the VLESS UUID + Reality keys; remove it now
    // that the tunnel is torn down, whatever the outcome.
    let _ = std::fs::remove_file(&client_config_path);

    result
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn sample_config() -> super::super::VlessConfig {
    super::super::VlessConfig {
      address: "vpn.example.com".to_string(),
      port: 443,
      uuid: "5fd0aa4f-7ca0-4b67-b2f0-5f2d8cf6a1df".to_string(),
      security: super::super::VlessSecurity::Tls,
      flow: "xtls-rprx-vision".to_string(),
      fingerprint: Some("chrome".to_string()),
      server_name: Some("vpn.example.com".to_string()),
      public_key: None,
      short_id: None,
      spider_x: None,
    }
  }

  #[test]
  fn test_write_client_config_produces_valid_doc() {
    let worker = XrayWorker::new(sample_config(), 10880);
    let path = worker.write_client_config("test-id").unwrap();
    let content = std::fs::read_to_string(&path).unwrap();
    let doc: serde_json::Value = serde_json::from_str(&content).unwrap();
    assert_eq!(doc["inbounds"][0]["listen"], "127.0.0.1");
    assert_eq!(doc["inbounds"][0]["port"], 10880);
    assert_eq!(doc["outbounds"][0]["protocol"], "vless");
    assert_eq!(
      doc["outbounds"][0]["settings"]["vnext"][0]["address"],
      "vpn.example.com"
    );
    std::fs::remove_file(&path).ok();
  }

  #[test]
  fn test_local_url_is_socks5_loopback() {
    let worker = XrayWorker::new(sample_config(), 10881);
    assert_eq!(worker.local_url(), "socks5://127.0.0.1:10881");
  }
}
