use std::time::Duration;

use serde_json::{json, Value};

use crate::chromium_manager::ChromiumManager;
use crate::human_typing::{MarkovTyper, TypingAction};
use crate::profile::{BrowserProfile, ProfileManager};

#[derive(Debug)]
pub struct CdpError {
  pub code: i32,
  pub message: String,
}

impl CdpError {
  pub fn new(code: i32, message: impl Into<String>) -> Self {
    Self {
      code,
      message: message.into(),
    }
  }
}

/// Verbatim port of the CDP/WebSocket utilities formerly inlined in
/// mcp_server.rs, so the MCP server and the upcoming scheduler/agent
/// executors share a single implementation.
#[derive(Default)]
pub struct CdpSession;

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
  }
}
