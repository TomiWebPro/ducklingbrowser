use crate::proxy_runner::find_sidecar_executable;
use crate::proxy_storage::is_process_running;
use crate::vpn_worker_storage::{
  delete_vpn_worker_config, find_vpn_worker_by_vpn_id, generate_vpn_worker_id,
  get_vpn_worker_config, list_vpn_worker_configs, save_vpn_worker_config, vpn_worker_config_path,
  VpnWorkerConfig,
};
use std::process::Stdio;

const VPN_WORKER_POLL_INTERVAL_MS: u64 = 100;
const VPN_WORKER_STARTUP_TIMEOUT_MS: u64 = 30_000;

/// Write a file with owner-only permissions from creation (no chmod window).
fn write_owner_only(path: &str, contents: &[u8]) -> std::io::Result<()> {
  #[cfg(unix)]
  {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
      .write(true)
      .create(true)
      .truncate(true)
      .mode(0o600)
      .open(path)?;
    file.write_all(contents)?;
    Ok(())
  }
  #[cfg(not(unix))]
  {
    std::fs::write(path, contents)
  }
}

async fn vpn_worker_accepting_connections(config: &VpnWorkerConfig) -> bool {
  let Some(port) = config.local_port else {
    return false;
  };

  if config
    .local_url
    .as_ref()
    .is_none_or(|local_url| local_url.is_empty())
  {
    return false;
  }

  matches!(
    tokio::time::timeout(
      tokio::time::Duration::from_millis(VPN_WORKER_POLL_INTERVAL_MS),
      tokio::net::TcpStream::connect(("127.0.0.1", port)),
    )
    .await,
    Ok(Ok(_))
  )
}

fn worker_log_path(id: &str) -> std::path::PathBuf {
  std::env::temp_dir().join(format!("duckling-vpn-{}.log", id))
}

fn read_worker_log(id: &str) -> String {
  std::fs::read_to_string(worker_log_path(id)).unwrap_or_else(|_| "No log available".to_string())
}

async fn wait_for_vpn_worker_ready(
  id: &str,
) -> Result<VpnWorkerConfig, Box<dyn std::error::Error>> {
  let startup_timeout = tokio::time::Duration::from_millis(VPN_WORKER_STARTUP_TIMEOUT_MS);
  let startup_deadline = tokio::time::Instant::now() + startup_timeout;

  tokio::time::sleep(tokio::time::Duration::from_millis(
    VPN_WORKER_POLL_INTERVAL_MS,
  ))
  .await;

  let mut attempts = 0u32;

  loop {
    tokio::time::sleep(tokio::time::Duration::from_millis(
      VPN_WORKER_POLL_INTERVAL_MS,
    ))
    .await;

    if let Some(updated_config) = get_vpn_worker_config(id) {
      let process_running = updated_config.pid.map(is_process_running).unwrap_or(false);

      if !process_running && attempts > 2 {
        let log_output = read_worker_log(id);
        let config_file_path = updated_config.config_file_path.clone();
        delete_vpn_worker_config(id);
        let _ = std::fs::remove_file(&config_file_path);
        return Err(format!("VPN worker process crashed. Log output:\n{}", log_output).into());
      }

      if vpn_worker_accepting_connections(&updated_config).await {
        return Ok(updated_config);
      }
    }

    attempts += 1;
    if tokio::time::Instant::now() >= startup_deadline {
      if let Some(config) = get_vpn_worker_config(id) {
        let process_running = config.pid.map(is_process_running).unwrap_or(false);
        let log_output = read_worker_log(id);
        // If the process is still alive, keep its config so a later
        // stop_vpn_worker can find and kill it; only remove the tracked
        // config (and the plaintext .conf) when it is confirmed dead.
        if !process_running {
          let config_file_path = config.config_file_path.clone();
          delete_vpn_worker_config(id);
          let _ = std::fs::remove_file(&config_file_path);
        }
        return Err(
          format!(
            "VPN worker failed to start within {:.1}s. pid={:?}, process_running={}, local_url={:?}\n\nVPN worker log:\n{}",
            startup_timeout.as_secs_f32(),
            config.pid,
            process_running,
            config.local_url,
            log_output
          )
          .into(),
        );
      }

      delete_vpn_worker_config(id);
      return Err("VPN worker config not found after spawn".into());
    }
  }
}

pub async fn start_vpn_worker(vpn_id: &str) -> Result<VpnWorkerConfig, Box<dyn std::error::Error>> {
  crate::proxy_runner::ensure_sidecar_version().await?;

  for config in list_vpn_worker_configs() {
    if let Some(pid) = config.pid {
      if !is_process_running(pid) {
        delete_vpn_worker_config(&config.id);
      }
    } else {
      delete_vpn_worker_config(&config.id);
    }
  }

  // Check if a VPN worker for this vpn_id already exists and is running
  if let Some(existing) = find_vpn_worker_by_vpn_id(vpn_id) {
    if let Some(pid) = existing.pid {
      if is_process_running(pid) {
        if vpn_worker_accepting_connections(&existing).await {
          return Ok(existing);
        }

        return wait_for_vpn_worker_ready(&existing.id).await;
      }
    }
    // Worker config exists but process is dead, clean up
    delete_vpn_worker_config(&existing.id);
  }

  // Load VPN config from storage to determine type
  let vpn_config = {
    let storage = crate::vpn::VPN_STORAGE
      .lock()
      .map_err(|e| format!("Failed to lock VPN storage: {e}"))?;
    storage
      .load_config(vpn_id)
      .map_err(|e| format!("Failed to load VPN config: {e}"))?
  };

  let vpn_type_str = match vpn_config.vpn_type {
    crate::vpn::VpnType::WireGuard => "wireguard",
    crate::vpn::VpnType::Vless => "vless",
  };

  // For VLESS, ensure the xray binary is installed in the parent before the
  // detached worker spawns. The worker also calls ensure_xray_binary() (a
  // no-op once installed), but downloading inside the worker would race the
  // parent's startup timeout and orphan the worker on slow networks.
  if vpn_config.vpn_type == crate::vpn::VpnType::Vless {
    crate::vpn::xray_vendor::ensure_xray_binary()
      .await
      .map_err(|e| format!("Failed to install Xray binary: {e}"))?;
  }

  // Write decrypted config to a temp file, restricted to the owner from the
  // moment of creation (no 0644->0600 chmod window leaking key material).
  let config_file_path = std::env::temp_dir()
    .join(format!("duckling_vpn_{}.conf", vpn_id))
    .to_string_lossy()
    .to_string();

  write_owner_only(&config_file_path, vpn_config.config_data.as_bytes())?;

  let id = generate_vpn_worker_id();

  // Find an available port
  let local_port = {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    listener.local_addr()?.port()
  };

  let config = VpnWorkerConfig::new(
    id.clone(),
    vpn_id.to_string(),
    vpn_type_str.to_string(),
    config_file_path.clone(),
  );
  save_vpn_worker_config(&config)?;

  let config_json_path = vpn_worker_config_path(&id);

  // Spawn detached VPN worker process
  let exe = find_sidecar_executable("duckling-proxy")?;

  #[cfg(unix)]
  {
    use std::os::unix::process::CommandExt;
    use std::process::Command as StdCommand;

    let mut cmd = StdCommand::new(&exe);
    cmd.arg("vpn-worker");
    cmd.arg("start");
    cmd.arg("--id");
    cmd.arg(&id);
    cmd.arg("--port");
    cmd.arg(local_port.to_string());
    cmd.arg("--config-path");
    cmd.arg(&config_json_path);

    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::null());

    let log_path = std::env::temp_dir().join(format!("duckling-vpn-{}.log", id));
    if let Ok(file) = std::fs::File::create(&log_path) {
      log::info!("VPN worker stderr will be logged to: {:?}", log_path);
      cmd.stderr(Stdio::from(file));
    } else {
      cmd.stderr(Stdio::null());
    }

    unsafe {
      cmd.pre_exec(|| {
        libc::setsid();
        if libc::setpriority(libc::PRIO_PROCESS, 0, -10) != 0 {
          let _ = libc::setpriority(libc::PRIO_PROCESS, 0, -5);
        }
        Ok(())
      });
    }

    let child = cmd.spawn().inspect_err(|_| {
      delete_vpn_worker_config(&id);
      let _ = std::fs::remove_file(&config_file_path);
    })?;
    let pid = child.id();

    let mut config_with_pid = config.clone();
    config_with_pid.pid = Some(pid);
    config_with_pid.local_port = Some(local_port);
    save_vpn_worker_config(&config_with_pid).inspect_err(|_| {
      let _ = std::fs::remove_file(&config_file_path);
    })?;

    drop(child);
  }

  #[cfg(windows)]
  {
    use std::os::windows::process::CommandExt;
    use std::process::Command as StdCommand;

    let mut cmd = StdCommand::new(&exe);
    cmd.arg("vpn-worker");
    cmd.arg("start");
    cmd.arg("--id");
    cmd.arg(&id);
    cmd.arg("--port");
    cmd.arg(local_port.to_string());
    cmd.arg("--config-path");
    cmd.arg(&config_json_path);

    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::null());

    let log_path = std::env::temp_dir().join(format!("duckling-vpn-{}.log", id));
    if let Ok(file) = std::fs::File::create(&log_path) {
      log::info!("VPN worker stderr will be logged to: {:?}", log_path);
      cmd.stderr(Stdio::from(file));
    } else {
      cmd.stderr(Stdio::null());
    }

    const DETACHED_PROCESS: u32 = 0x00000008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x00000200;
    const CREATE_NO_WINDOW: u32 = 0x08000000;
    cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);

    let child = cmd.spawn().inspect_err(|_| {
      delete_vpn_worker_config(&id);
      let _ = std::fs::remove_file(&config_file_path);
    })?;
    let pid = child.id();

    let mut config_with_pid = config.clone();
    config_with_pid.pid = Some(pid);
    config_with_pid.local_port = Some(local_port);
    save_vpn_worker_config(&config_with_pid).inspect_err(|_| {
      let _ = std::fs::remove_file(&config_file_path);
    })?;

    drop(child);
  }

  wait_for_vpn_worker_ready(&id).await
}

pub async fn stop_vpn_worker(id: &str) -> Result<bool, Box<dyn std::error::Error>> {
  let config = get_vpn_worker_config(id);

  if let Some(config) = config {
    if let Some(pid) = config.pid {
      #[cfg(unix)]
      {
        use std::process::Command;
        let _ = Command::new("kill")
          .arg("-TERM")
          .arg(pid.to_string())
          .output();
      }
      #[cfg(windows)]
      {
        use std::os::windows::process::CommandExt;
        use std::process::Command;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        let _ = Command::new("taskkill")
          .args(["/F", "/PID", &pid.to_string()])
          .creation_flags(CREATE_NO_WINDOW)
          .output();
      }

      tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

      // Escalate to SIGKILL if the worker ignored SIGTERM, so a hung worker
      // can't keep the port bound or linger as an untracked orphan.
      if is_process_running(pid) {
        #[cfg(unix)]
        {
          use std::process::Command;
          let _ = Command::new("kill")
            .arg("-KILL")
            .arg(pid.to_string())
            .output();
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
      }
    }

    // Clean up temp config file, the xray client JSON (contains the VLESS
    // UUID + Reality keys), and the worker/xray logs.
    let _ = std::fs::remove_file(&config.config_file_path);
    let _ = std::fs::remove_file(std::env::temp_dir().join(format!("duckling_xray_{id}.json")));
    let _ = std::fs::remove_file(std::env::temp_dir().join(format!("duckling-xray-{id}.log")));
    let _ = std::fs::remove_file(worker_log_path(id));

    delete_vpn_worker_config(id);
    return Ok(true);
  }

  Ok(false)
}

pub async fn stop_vpn_worker_by_vpn_id(vpn_id: &str) -> Result<bool, Box<dyn std::error::Error>> {
  if let Some(config) = find_vpn_worker_by_vpn_id(vpn_id) {
    return stop_vpn_worker(&config.id).await;
  }
  Ok(false)
}

pub async fn stop_all_vpn_workers() -> Result<(), Box<dyn std::error::Error>> {
  let configs = list_vpn_worker_configs();
  for config in configs {
    let _ = stop_vpn_worker(&config.id).await;
  }
  Ok(())
}
