use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use serde_json::{json, Value};

use crate::chromium_manager::ChromiumManager;
use crate::human_typing::{MarkovTyper, TypingAction};
use crate::profile::{BrowserProfile, ProfileManager};

#[derive(Debug)]
pub struct CdpError {
  pub code: i32,
  pub message: String,
  /// True when the error reflects a dead WebSocket (transport) rather than a
  /// browser-level command error. Transport failures are retried via a fresh
  /// connection; command errors are returned as-is.
  pub transport: bool,
}

impl CdpError {
  pub fn new(code: i32, message: impl Into<String>) -> Self {
    Self {
      code,
      message: message.into(),
      transport: false,
    }
  }

  fn transport_err(message: impl Into<String>) -> Self {
    Self {
      code: -32000,
      message: message.into(),
      transport: true,
    }
  }
}

/// Verbatim port of the CDP/WebSocket utilities formerly inlined in
/// mcp_server.rs, so the MCP server and the scheduler/agent executors share a
/// single implementation.
///
/// Plain `send_cdp` calls multiplex over one persistent WebSocket per
/// (profile, tab) (keyed by the target's `webSocketDebuggerUrl`), so repeated
/// DOM commands don't pay a connect per call. Connections that die ("stale")
/// fall back to the legacy per-call connect. Event-streaming / ordering
/// flows (`send_human_keystrokes`, `send_cdp_and_wait_for_load`) still use
/// their own dedicated connection.
#[derive(Default, Clone)]
pub struct CdpSession;

/// One open CDP WebSocket: commands are serialized on an unbounded channel
/// to a reader task that fans responses out to pending requests by id.
struct PooledConnection {
  next_id: AtomicU64,
  writer: tokio::sync::mpsc::UnboundedSender<String>,
  pending: Arc<PendingMap>,
  reader: tokio::task::JoinHandle<()>,
}

#[derive(Default)]
struct CdpPool {
  connections: tokio::sync::Mutex<HashMap<String, Arc<PooledConnection>>>,
  /// Serializes concurrent first-opens of the same url so only one opener
  /// actually connects while the rest wait for it (and then reuse the result).
  connect_locks: tokio::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

static CDP_POOL: LazyLock<CdpPool> = LazyLock::new(CdpPool::default);

/// Per-request timeout so a browser that stops answering never leaks an
/// unbounded pending-map entry.
const POOL_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

type PendingMap =
  std::sync::Mutex<HashMap<u64, tokio::sync::oneshot::Sender<Result<Value, CdpError>>>>;

impl CdpPool {
  async fn lookup(&self, ws_url: &str) -> Option<Arc<PooledConnection>> {
    self.connections.lock().await.get(ws_url).cloned()
  }

  /// Remove a specific stale connection (only if it is still the pooled one).
  async fn evict(&self, ws_url: &str, conn: &Arc<PooledConnection>) {
    let mut guard = self.connections.lock().await;
    if guard
      .get(ws_url)
      .map(|c| Arc::ptr_eq(c, conn))
      .unwrap_or(false)
    {
      guard.remove(ws_url);
      conn.reader.abort();
    }
  }

  /// Connect a fresh WebSocket for `ws_url` and register it in the pool.
  async fn open(&self, ws_url: &str) -> Result<Arc<PooledConnection>, CdpError> {
    // Serialize concurrent first-opens of the same target URL: exactly one
    // opener dials the socket while the others wait and reuse the result.
    let connect_lock = {
      let mut locks = self.connect_locks.lock().await;
      locks
        .entry(ws_url.to_string())
        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone()
    };
    let _permit = connect_lock.lock().await;

    if let Some(existing) = self.lookup(ws_url).await {
      return Ok(existing);
    }

    let conn = self.open_conn(ws_url).await?;
    let mut guard = self.connections.lock().await;
    let pooled = guard
      .entry(ws_url.to_string())
      .or_insert_with(|| conn.clone());
    Ok(pooled.clone())
  }

  /// Actually dial one CDP WebSocket and spawn its reader loop.
  async fn open_conn(&self, ws_url: &str) -> Result<Arc<PooledConnection>, CdpError> {
    use tokio_tungstenite::connect_async;

    let (ws_stream, _) = connect_async(ws_url)
      .await
      .map_err(|e| CdpError::new(-32000, format!("Failed to connect to CDP WebSocket: {e}")))?;

    let (writer, command_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let pending: Arc<PendingMap> = Arc::new(PendingMap::default());
    let reader = tokio::spawn(cdp_reader_loop(ws_stream, command_rx, pending.clone()));
    Ok(Arc::new(PooledConnection {
      next_id: AtomicU64::new(1),
      writer,
      pending,
      reader,
    }))
  }
}

/// Routes commands to the wire and CDP responses back to their pending
/// request. Exiting (stream close/error) fails every still-pending request so
/// callers fall back to a fresh connection.
async fn cdp_reader_loop<S>(
  ws: tokio_tungstenite::WebSocketStream<S>,
  mut command_rx: tokio::sync::mpsc::UnboundedReceiver<String>,
  pending: Arc<PendingMap>,
) where
  S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
  use futures_util::sink::SinkExt;
  use futures_util::stream::StreamExt;
  use tokio_tungstenite::tungstenite::Message;

  let (mut sink, mut stream) = ws.split();

  loop {
    tokio::select! {
      biased;

      command = command_rx.recv() => {
        let Some(text) = command else {
          break; // every writer dropped
        };
        if let Err(e) = sink.send(Message::Text(text.into())).await {
          fail_all_pending(&pending, format!("CDP send error: {e}"));
          break;
        }
      }
      incoming = stream.next() => match incoming {
        Some(Ok(Message::Text(text))) => {
          let Ok(reply) = serde_json::from_str::<Value>(text.as_str()) else {
            continue;
          };
          let Some(id) = reply.get("id").and_then(Value::as_u64) else {
            continue; // event frame, not a request response
          };
          let tx = pending.lock().unwrap_or_else(|p| p.into_inner()).remove(&id);
          if let Some(tx) = tx {
            let result = match reply.get("error") {
              Some(err) => Err(CdpError::new(-32000, format!("CDP error: {err}"))),
              None => Ok(reply.get("result").cloned().unwrap_or_else(|| json!({}))),
            };
            let _ = tx.send(result);
          }
        }
        Some(Ok(_)) => {} // ping/pong/close/binary frames
        Some(Err(e)) => {
          fail_all_pending(&pending, format!("CDP WebSocket error: {e}"));
          break;
        }
        None => {
          fail_all_pending(&pending, "CDP connection closed".to_string());
          break;
        }
      },
    }
  }
}

fn fail_all_pending(pending: &PendingMap, message: String) {
  let mut guard = pending
    .lock()
    .unwrap_or_else(|poisoned| poisoned.into_inner());
  for (_, tx) in guard.drain() {
    let _ = tx.send(Err(CdpError::transport_err(message.clone())));
  }
}

impl PooledConnection {
  /// Multiplex a single CDP command over the shared connection, waiting for
  /// the response that carries this request's id.
  async fn request(&self, method: &str, params: Value) -> Result<Value, CdpError> {
    let id = self.next_id.fetch_add(1, Ordering::Relaxed);
    let (tx, rx) = tokio::sync::oneshot::channel::<Result<Value, CdpError>>();
    self
      .pending
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner())
      .insert(id, tx);
    let command = json!({ "id": id, "method": method, "params": params });
    if self.writer.send(command.to_string()).is_err() {
      self
        .pending
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .remove(&id);
      return Err(CdpError::transport_err("CDP connection closed"));
    }

    match tokio::time::timeout(POOL_REQUEST_TIMEOUT, rx).await {
      Ok(Ok(result)) => result,
      Ok(Err(_)) => Err(CdpError::transport_err("CDP connection closed")),
      Err(_) => {
        self
          .pending
          .lock()
          .unwrap_or_else(|poisoned| poisoned.into_inner())
          .remove(&id);
        Err(CdpError::new(-32000, "CDP request timed out"))
      }
    }
  }
}

impl CdpSession {
  pub fn new() -> Self {
    Self
  }

  pub async fn get_cdp_port_for_profile(&self, profile: &BrowserProfile) -> Result<u16, CdpError> {
    let profiles_dir = ProfileManager::instance().get_profiles_dir();
    let profile_path = profile.get_profile_data_path(&profiles_dir);
    let profile_path_str = profile_path.to_string_lossy();

    // Retry a few times — port info may not be stored yet right after launch
    for attempt in 0..10 {
      if attempt > 0 {
        tokio::time::sleep(Duration::from_secs(1)).await;
      }
      let port = if profile.browser == "chromium" {
        ChromiumManager::instance()
          .get_cdp_port(&profile_path_str)
          .await
      } else {
        None
      };
      if let Some(p) = port {
        return Ok(p);
      }
    }

    Err(CdpError::new(
      -32000,
      format!(
        "No CDP connection available for profile '{}'. Make sure the browser is running.",
        profile.name
      ),
    ))
  }

  pub async fn get_cdp_ws_url(&self, port: u16) -> Result<String, CdpError> {
    let url = format!("http://127.0.0.1:{port}/json");
    let client = reqwest::Client::new();

    // Retry connecting to CDP endpoint (browser may still be starting up)
    let max_attempts = 15;
    let mut last_err = String::new();
    for attempt in 0..max_attempts {
      if attempt > 0 {
        tokio::time::sleep(Duration::from_secs(1)).await;
      }
      match client
        .get(&url)
        .timeout(Duration::from_secs(3))
        .send()
        .await
      {
        Ok(resp) => match resp.json::<Vec<Value>>().await {
          Ok(targets) => {
            if let Some(ws_url) = targets
              .iter()
              .find(|t| t.get("type").and_then(|v| v.as_str()) == Some("page"))
              .and_then(|t| t.get("webSocketDebuggerUrl"))
              .and_then(|v| v.as_str())
            {
              return Ok(ws_url.to_string());
            }
            last_err = "No page target found in browser".to_string();
          }
          Err(e) => {
            last_err = format!("Failed to parse CDP targets: {e}");
          }
        },
        Err(e) => {
          last_err = format!("Failed to connect to browser CDP endpoint: {e}");
        }
      }
    }

    Err(CdpError::new(-32000, last_err))
  }

  /// Convenience: profile → CDP WebSocket URL in one call.
  #[allow(dead_code)]
  pub async fn resolve_ws_url(&self, profile: &BrowserProfile) -> Result<String, CdpError> {
    let port = self.get_cdp_port_for_profile(profile).await?;
    self.get_cdp_ws_url(port).await
  }

  pub async fn send_cdp(
    &self,
    ws_url: &str,
    method: &str,
    params: Value,
  ) -> Result<Value, CdpError> {
    // Fast path: a healthy pooled connection for this webSocketDebuggerUrl.
    if let Some(conn) = CDP_POOL.lookup(ws_url).await {
      match conn.request(method, params.clone()).await {
        Ok(value) => return Ok(value),
        Err(e) if e.transport => {
          // Stale connection: evict it and fall through to a fresh one.
          CDP_POOL.evict(ws_url, &conn).await;
        }
        Err(e) => return Err(e),
      }
    }

    // (Re)connect through the pool. Any failure falls back to the legacy
    // per-call connection so a transient disconnect never breaks one call.
    match CDP_POOL.open(ws_url).await {
      Ok(conn) => match conn.request(method, params.clone()).await {
        Ok(value) => Ok(value),
        Err(e) if e.transport => {
          CDP_POOL.evict(ws_url, &conn).await;
          self.send_cdp_direct(ws_url, method, params).await
        }
        Err(e) => Err(e),
      },
      Err(open_err) => {
        log::debug!("CDP pool open failed for {ws_url}: {open_err:?}; using direct connect");
        self.send_cdp_direct(ws_url, method, params).await
      }
    }
  }

  /// Legacy single-command connection: connect, send one command, wait for
  /// its response, close. Used as the fallback when the pool is stale.
  async fn send_cdp_direct(
    &self,
    ws_url: &str,
    method: &str,
    params: Value,
  ) -> Result<Value, CdpError> {
    use futures_util::sink::SinkExt;
    use futures_util::stream::StreamExt;
    use tokio_tungstenite::connect_async;
    use tokio_tungstenite::tungstenite::Message;

    let (mut ws_stream, _) = connect_async(ws_url)
      .await
      .map_err(|e| CdpError::new(-32000, format!("Failed to connect to CDP WebSocket: {e}")))?;

    let command = json!({
      "id": 1,
      "method": method,
      "params": params
    });

    ws_stream
      .send(Message::Text(command.to_string().into()))
      .await
      .map_err(|e| CdpError::new(-32000, format!("Failed to send CDP command: {e}")))?;

    while let Some(msg) = ws_stream.next().await {
      let msg = msg.map_err(|e| CdpError::new(-32000, format!("CDP WebSocket error: {e}")))?;
      if let Message::Text(text) = msg {
        let response: Value = serde_json::from_str(text.as_str())
          .map_err(|e| CdpError::new(-32000, format!("Failed to parse CDP response: {e}")))?;
        if response.get("id") == Some(&json!(1)) {
          if let Some(error) = response.get("error") {
            return Err(CdpError::new(-32000, format!("CDP error: {error}")));
          }
          return Ok(response.get("result").cloned().unwrap_or_else(|| json!({})));
        }
      }
    }

    Err(CdpError::new(-32000, "No response received from CDP"))
  }

  pub async fn send_human_keystrokes(
    &self,
    ws_url: &str,
    text: &str,
    wpm: Option<f64>,
  ) -> Result<(), CdpError> {
    use futures_util::sink::SinkExt;
    use futures_util::stream::StreamExt;
    use tokio_tungstenite::connect_async;
    use tokio_tungstenite::tungstenite::Message;

    let events = MarkovTyper::new(text, wpm).run();

    let (mut ws_stream, _) = connect_async(ws_url)
      .await
      .map_err(|e| CdpError::new(-32000, format!("Failed to connect to CDP WebSocket: {e}")))?;

    let mut cmd_id = 1u64;
    let mut last_time = 0.0;

    for event in &events {
      let delay = event.time - last_time;
      if delay > 0.0 {
        tokio::time::sleep(Duration::from_secs_f64(delay)).await;
      }
      last_time = event.time;

      match &event.action {
        TypingAction::Char(ch) => {
          let text_str = ch.to_string();
          // keyDown
          let down = json!({
            "id": cmd_id,
            "method": "Input.dispatchKeyEvent",
            "params": {
              "type": "keyDown",
              "text": text_str,
              "key": text_str,
              "unmodifiedText": text_str,
            }
          });
          cmd_id += 1;
          ws_stream
            .send(Message::Text(down.to_string().into()))
            .await
            .map_err(|e| CdpError::new(-32000, format!("Failed to send key event: {e}")))?;
          // Drain response
          let _ = ws_stream.next().await;

          // keyUp
          let up = json!({
            "id": cmd_id,
            "method": "Input.dispatchKeyEvent",
            "params": {
              "type": "keyUp",
              "key": text_str,
            }
          });
          cmd_id += 1;
          ws_stream
            .send(Message::Text(up.to_string().into()))
            .await
            .map_err(|e| CdpError::new(-32000, format!("Failed to send key event: {e}")))?;
          let _ = ws_stream.next().await;
        }
        TypingAction::Backspace => {
          let down = json!({
            "id": cmd_id,
            "method": "Input.dispatchKeyEvent",
            "params": {
              "type": "keyDown",
              "key": "Backspace",
              "code": "Backspace",
              "windowsVirtualKeyCode": 8,
              "nativeVirtualKeyCode": 8,
            }
          });
          cmd_id += 1;
          ws_stream
            .send(Message::Text(down.to_string().into()))
            .await
            .map_err(|e| CdpError::new(-32000, format!("Failed to send key event: {e}")))?;
          let _ = ws_stream.next().await;

          let up = json!({
            "id": cmd_id,
            "method": "Input.dispatchKeyEvent",
            "params": {
              "type": "keyUp",
              "key": "Backspace",
              "code": "Backspace",
              "windowsVirtualKeyCode": 8,
              "nativeVirtualKeyCode": 8,
            }
          });
          cmd_id += 1;
          ws_stream
            .send(Message::Text(up.to_string().into()))
            .await
            .map_err(|e| CdpError::new(-32000, format!("Failed to send key event: {e}")))?;
          let _ = ws_stream.next().await;
        }
      }
    }

    Ok(())
  }

  /// Send a CDP command and wait for the page to finish loading.
  /// Uses a single WebSocket connection to: enable Page events, send the command,
  /// wait for the command response, then wait for `Page.loadEventFired`.
  pub async fn send_cdp_and_wait_for_load(
    &self,
    ws_url: &str,
    method: &str,
    params: Value,
    timeout_secs: u64,
  ) -> Result<Value, CdpError> {
    use futures_util::sink::SinkExt;
    use futures_util::stream::StreamExt;
    use tokio_tungstenite::connect_async;
    use tokio_tungstenite::tungstenite::Message;

    let (mut ws_stream, _) = connect_async(ws_url)
      .await
      .map_err(|e| CdpError::new(-32000, format!("Failed to connect to CDP WebSocket: {e}")))?;

    // Enable Page domain events so we receive loadEventFired
    let enable_cmd = json!({
      "id": 1,
      "method": "Page.enable",
      "params": {}
    });
    ws_stream
      .send(Message::Text(enable_cmd.to_string().into()))
      .await
      .map_err(|e| CdpError::new(-32000, format!("Failed to send Page.enable: {e}")))?;

    // Wait for Page.enable response
    loop {
      let msg = ws_stream
        .next()
        .await
        .ok_or_else(|| CdpError::new(-32000, "WebSocket closed waiting for Page.enable response"))?
        .map_err(|e| CdpError::new(-32000, format!("CDP WebSocket error: {e}")))?;
      if let Message::Text(text) = msg {
        let resp: Value = serde_json::from_str(text.as_str()).unwrap_or_default();
        if resp.get("id") == Some(&json!(1)) {
          break;
        }
      }
    }

    // Send the actual command (e.g., Page.navigate)
    let command = json!({
      "id": 2,
      "method": method,
      "params": params
    });
    ws_stream
      .send(Message::Text(command.to_string().into()))
      .await
      .map_err(|e| CdpError::new(-32000, format!("Failed to send CDP command: {e}")))?;

    // Wait for command response and then for Page.loadEventFired
    let mut command_result = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_secs);

    loop {
      let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
      if remaining.is_zero() {
        // Timed out waiting for load — return the command result if we have it
        break;
      }

      let msg = match tokio::time::timeout(remaining, ws_stream.next()).await {
        Ok(Some(Ok(msg))) => msg,
        Ok(Some(Err(e))) => {
          return Err(CdpError::new(-32000, format!("CDP WebSocket error: {e}")));
        }
        Ok(None) => break, // stream ended
        Err(_) => break,   // timeout
      };

      if let Message::Text(text) = msg {
        let response: Value = serde_json::from_str(text.as_str()).unwrap_or_default();

        // Check for command response
        if response.get("id") == Some(&json!(2)) {
          if let Some(error) = response.get("error") {
            return Err(CdpError::new(-32000, format!("CDP error: {error}")));
          }
          command_result = Some(response.get("result").cloned().unwrap_or_else(|| json!({})));
        }

        // Check for Page.loadEventFired — page is fully loaded
        if response.get("method") == Some(&json!("Page.loadEventFired")) {
          break;
        }
      }
    }

    // Disable Page domain events
    let disable_cmd = json!({
      "id": 3,
      "method": "Page.disable",
      "params": {}
    });
    let _ = ws_stream
      .send(Message::Text(disable_cmd.to_string().into()))
      .await;

    command_result.ok_or_else(|| CdpError::new(-32000, "No response received from CDP"))
  }

  pub fn get_running_profile(&self, profile_id: &str) -> Result<BrowserProfile, CdpError> {
    let profiles = ProfileManager::instance()
      .list_profiles()
      .map_err(|e| CdpError::new(-32000, format!("Failed to list profiles: {e}")))?;

    let profile = profiles
      .into_iter()
      .find(|p| p.id.to_string() == profile_id)
      .ok_or_else(|| CdpError::new(-32000, format!("Profile not found: {profile_id}")))?;

    if profile.browser != "chromium" {
      return Err(CdpError::new(
        -32000,
        "MCP only supports Chromium profiles".to_string(),
      ));
    }

    if profile.process_id.is_none() {
      return Err(CdpError::new(
        -32000,
        format!("Profile '{}' is not running", profile.name),
      ));
    }

    Ok(profile)
  }

  /// Poll `Runtime.evaluate` until `document.querySelector(selector)` matches
  /// or the timeout elapses.
  #[allow(dead_code)]
  pub async fn wait_for_selector(
    &self,
    ws_url: &str,
    selector: &str,
    timeout_ms: u64,
  ) -> Result<(), CdpError> {
    let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);
    let expression = selector_expression(selector);

    loop {
      let result = self
        .send_cdp(
          ws_url,
          "Runtime.evaluate",
          json!({
            "expression": expression,
            "returnByValue": true
          }),
        )
        .await?;
      let found = result
        .pointer("/result/value")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
      if found {
        return Ok(());
      }
      if tokio::time::Instant::now() >= deadline {
        return Err(CdpError::new(
          -32000,
          format!("Timed out waiting for selector '{selector}'"),
        ));
      }
      tokio::time::sleep(Duration::from_millis(250)).await;
    }
  }
}

/// Build the `Runtime.evaluate` expression that checks whether a selector
/// matches, JSON-escaping the selector so quotes in it are safe.
#[allow(dead_code)]
fn selector_expression(selector: &str) -> String {
  let escaped = serde_json::to_string(selector).unwrap_or_default();
  format!("!!document.querySelector({escaped})")
}

#[allow(dead_code)]
#[async_trait::async_trait]
pub trait CdpSessionTrait: Send {
  async fn resolve_ws_url(&mut self, profile: &BrowserProfile) -> Result<String, CdpError>;
  async fn navigate(&mut self, ws_url: &str, url: &str) -> Result<Value, CdpError>;
  async fn evaluate(&mut self, ws_url: &str, expression: &str) -> Result<Value, CdpError>;
  async fn screenshot(&mut self, ws_url: &str) -> Result<Value, CdpError>;
  async fn wait_for_selector(
    &mut self,
    ws_url: &str,
    selector: &str,
    timeout_ms: u64,
  ) -> Result<(), CdpError>;
}

#[async_trait::async_trait]
impl CdpSessionTrait for CdpSession {
  async fn resolve_ws_url(&mut self, profile: &BrowserProfile) -> Result<String, CdpError> {
    self.resolve_ws_url(profile).await
  }

  async fn navigate(&mut self, ws_url: &str, url: &str) -> Result<Value, CdpError> {
    self
      .send_cdp(ws_url, "Page.navigate", json!({ "url": url }))
      .await
  }

  async fn evaluate(&mut self, ws_url: &str, expression: &str) -> Result<Value, CdpError> {
    self
      .send_cdp(
        ws_url,
        "Runtime.evaluate",
        json!({
          "expression": expression,
          "returnByValue": true
        }),
      )
      .await
  }

  async fn screenshot(&mut self, ws_url: &str) -> Result<Value, CdpError> {
    self
      .send_cdp(
        ws_url,
        "Page.captureScreenshot",
        json!({ "format": "png", "captureBeyondViewport": true }),
      )
      .await
  }

  async fn wait_for_selector(
    &mut self,
    ws_url: &str,
    selector: &str,
    timeout_ms: u64,
  ) -> Result<(), CdpError> {
    self.wait_for_selector(ws_url, selector, timeout_ms).await
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn selector_expression_escapes_quotes() {
    assert_eq!(
      selector_expression("a[href=\"/x\"]"),
      r#"!!document.querySelector("a[href=\"/x\"]")"#
    );
    assert_eq!(
      selector_expression("button.submit"),
      "!!document.querySelector(\"button.submit\")"
    );
  }

  #[test]
  fn cdp_error_carries_code_and_message() {
    let err = CdpError::new(-32602, "browser must be 'Chromium'");
    assert_eq!(err.code, -32602);
    assert_eq!(err.message, "browser must be 'Chromium'");
    assert!(!err.transport);
  }

  /// Spins up an in-process fake CDP endpoint: accepts one WebSocket, collects
  /// two commands, and answers them in REVERSE arrival order (newest command
  /// first) so the pool's id→result multiplexer must route each reply to the
  /// exact request that asked for it — a position/timing-based router would
  /// hand the wrong method to the wrong caller.
  #[test]
  fn pooled_multiplexer_routes_out_of_order_replies_to_the_right_request() {
    use futures_util::sink::SinkExt;
    use futures_util::stream::StreamExt;
    use tokio_tungstenite::tungstenite::Message;

    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
      let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
      let addr = listener.local_addr().unwrap();
      let ws_url = format!("ws://{addr}");

      let server = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
        // Each loop iteration handles a pair of in-flight commands.
        for _ in 0..2 {
          let mut commands = Vec::new();
          for _ in 0..2 {
            match ws.next().await {
              Some(Ok(Message::Text(text))) => {
                commands.push(serde_json::from_str::<Value>(text.as_str()).unwrap());
              }
              _ => return,
            }
          }
          // Reply to the NEWEST command first: id 2 then id 1 (and id 4 then
          // id 3), so the replies land on the wire out of arrival order.
          commands.reverse();
          for command in commands {
            let id = command["id"].as_u64().unwrap();
            let method = command["method"].as_str().unwrap().to_string();
            ws.send(Message::Text(
              json!({ "id": id, "result": { "method": method } })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();
            tokio::time::sleep(Duration::from_millis(5)).await;
          }
        }
      });

      let session = CdpSession::new();
      let call_a = session.send_cdp(&ws_url, "Dom.getDocument", json!({}));
      let call_b = session.send_cdp(&ws_url, "Runtime.evaluate", json!({ "expression": "1+1" }));
      let (a, b) = tokio::join!(call_a, call_b);
      // Despite the out-of-order replies, each caller gets its own result.
      assert_eq!(a.unwrap()["method"], "Dom.getDocument");
      assert_eq!(b.unwrap()["method"], "Runtime.evaluate");

      // The SAME pooled socket is reused for the next pair (no reconnect).
      let call_c = session.send_cdp(&ws_url, "Page.enable", json!({}));
      let call_d = session.send_cdp(&ws_url, "Page.reload", json!({ "ignoreCache": true }));
      let (c, d) = tokio::join!(call_c, call_d);
      assert_eq!(c.unwrap()["method"], "Page.enable");
      assert_eq!(d.unwrap()["method"], "Page.reload");

      let _ = server.await;
    });
  }

  /// A pooled connection whose socket closes mid-request surfaces a transport
  /// error and is evicted so the next call dials a fresh connection.
  #[test]
  fn stale_pool_connection_is_evicted_after_transport_error() {
    use futures_util::stream::StreamExt;

    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
      let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
      let addr = listener.local_addr().unwrap();
      let ws_url = format!("ws://{addr}");

      // Accept one connection, read one command, then close the socket without
      // replying so the pending request dies mid-flight.
      let server = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
        let _ = ws.next().await;
        let _ = ws.close(None).await;
      });

      let session = CdpSession::new();
      let result = session
        .send_cdp(&ws_url, "Runtime.evaluate", json!({ "expression": "1" }))
        .await;
      assert!(
        result.is_err(),
        "a request on a dying socket must not succeed"
      );

      // The stale connection was evicted from the pool (recovery happens
      // through the direct fallback, then a fresh connection on the next call).
      assert!(CDP_POOL.connections.lock().await.get(&ws_url).is_none());

      let _ = server.await;
    });
  }
}
