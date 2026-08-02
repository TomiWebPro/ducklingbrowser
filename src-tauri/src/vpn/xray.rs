//! VLESS (Reality / TLS) configuration parsing and Xray client generation.
//!
//! This module models a VLESS client configuration, parses the ecosystem
//! standard `vless://` share-link URI (as emitted by v2rayN, haha/nekoray,
//! etc.), and renders the JSON document fed to `xray run -c` when routing
//! traffic through a local SOCKS5 inbound. Reality and plain VLESS+TLS are
//! supported from the same model: Reality uses pbk/sid/spx, TLS omits them.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::config::VpnError;

/// The transport security for a VLESS connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum VlessSecurity {
  Reality,
  Tls,
}

impl VlessSecurity {
  pub fn as_str(self) -> &'static str {
    match self {
      VlessSecurity::Reality => "reality",
      VlessSecurity::Tls => "tls",
    }
  }
}

impl core::str::FromStr for VlessSecurity {
  type Err = VpnError;

  fn from_str(s: &str) -> Result<Self, Self::Err> {
    match s.trim().to_ascii_lowercase().as_str() {
      "reality" => Ok(VlessSecurity::Reality),
      "tls" => Ok(VlessSecurity::Tls),
      other => Err(VpnError::InvalidVless(format!(
        "Unknown security '{other}' (expected 'reality' or 'tls')"
      ))),
    }
  }
}

/// A parsed VLESS client configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VlessConfig {
  #[serde(alias = "host")]
  pub address: String,
  pub port: u16,
  #[serde(alias = "id")]
  pub uuid: String,
  pub security: VlessSecurity,
  #[serde(default = "default_flow")]
  pub flow: String,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub fingerprint: Option<String>,
  #[serde(
    default,
    alias = "sni",
    alias = "server_name",
    skip_serializing_if = "Option::is_none"
  )]
  pub server_name: Option<String>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub public_key: Option<String>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub short_id: Option<String>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub spider_x: Option<String>,
}

fn default_flow() -> String {
  "xtls-rprx-vision".to_string()
}

impl VlessConfig {
  /// The SNI used for the TLS handshake, falling back to the server address.
  pub fn resolved_server_name(&self) -> String {
    self
      .server_name
      .clone()
      .unwrap_or_else(|| self.address.clone())
  }

  /// Validate the configuration. Reality requires a public key and short ID;
  /// plain TLS only needs a server and a valid UUID.
  pub fn validate(&self) -> Result<(), VpnError> {
    if self.address.is_empty() {
      return Err(VpnError::InvalidVless("Missing server address".to_string()));
    }
    if self.port == 0 {
      return Err(VpnError::InvalidVless("Invalid server port".to_string()));
    }
    Uuid::parse_str(&self.uuid)
      .map_err(|e| VpnError::InvalidVless(format!("Invalid VLESS UUID '{}': {e}", self.uuid)))?;

    match self.security {
      VlessSecurity::Reality => {
        if self.public_key.as_deref().unwrap_or_default().is_empty() {
          return Err(VpnError::InvalidVless(
            "Reality config requires a public key (pbk)".to_string(),
          ));
        }
        if self.short_id.as_deref().unwrap_or_default().is_empty() {
          return Err(VpnError::InvalidVless(
            "Reality config requires a short ID (sid)".to_string(),
          ));
        }
      }
      VlessSecurity::Tls => {}
    }
    Ok(())
  }
}

/// Percent-decode `%XX` sequences in a share-link component.
fn percent_decode(s: &str) -> String {
  let bytes = s.as_bytes();
  let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
  let mut i = 0;
  let hex_val = |c: u8| -> Option<u8> {
    match c {
      b'0'..=b'9' => Some(c - b'0'),
      b'a'..=b'f' => Some(c - b'a' + 10),
      b'A'..=b'F' => Some(c - b'A' + 10),
      _ => None,
    }
  };
  while i < bytes.len() {
    if bytes[i] == b'%' && i + 2 < bytes.len() {
      if let (Some(h), Some(l)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
        out.push(h * 16 + l);
        i += 3;
        continue;
      }
    }
    out.push(bytes[i]);
    i += 1;
  }
  String::from_utf8_lossy(&out).into_owned()
}

/// Serialize a host for use in a URI authority. IPv6 literals are bracketed.
fn format_host(address: &str) -> String {
  if address.contains(':') && !address.starts_with('[') {
    format!("[{address}]")
  } else {
    address.to_string()
  }
}

/// Parse an ecosystem-standard `vless://` share link into a `VlessConfig`.
///
/// Supported query parameters: `security` (`reality`|`tls`), `encryption`
/// (must be `none`), `sni`, `fp`, `pbk`, `sid`, `spx`, `flow`. Any
/// unrecognized parameters are ignored so links from arbitrary clients parse.
pub fn parse_vless_uri(uri: &str) -> Result<VlessConfig, VpnError> {
  let trimmed = uri.trim().strip_prefix('\u{feff}').unwrap_or(uri.trim());
  let rest = trimmed
    .strip_prefix("vless://")
    .ok_or_else(|| VpnError::InvalidVless("Not a vless:// URI".to_string()))?;

  // Split off the `#name` fragment (ignored for configuration).
  let authority_and_query = match rest.split_once('#') {
    Some((a, _)) => a,
    None => rest,
  };

  let (authority, query) = match authority_and_query.split_once('?') {
    Some((a, q)) => (a, q),
    None => (authority_and_query, ""),
  };

  // authority is "{uuid}@{host:port}". IPv6 hosts are bracketed.
  let (uuid, host_port) = authority
    .split_once('@')
    .ok_or_else(|| VpnError::InvalidVless("Missing '@' between UUID and host".to_string()))?;

  let (address, port) = if let Some(inner) = host_port.strip_prefix('[') {
    let end = inner
      .find(']')
      .ok_or_else(|| VpnError::InvalidVless("Unterminated IPv6 host".to_string()))?;
    let host = &inner[..end];
    let rest = &inner[end + 1..];
    let port = rest
      .strip_prefix(':')
      .and_then(|p| p.parse::<u16>().ok())
      .ok_or_else(|| VpnError::InvalidVless("Invalid or missing port".to_string()))?;
    (host.to_string(), port)
  } else {
    let (host, p) = host_port
      .rsplit_once(':')
      .ok_or_else(|| VpnError::InvalidVless("Missing host:port".to_string()))?;
    let port = p
      .parse::<u16>()
      .map_err(|_| VpnError::InvalidVless(format!("Invalid port '{p}'")))?;
    (percent_decode(host), port)
  };

  let uuid = percent_decode(uuid);
  Uuid::parse_str(&uuid)
    .map_err(|e| VpnError::InvalidVless(format!("Invalid VLESS UUID '{uuid}': {e}")))?;

  // Parse query parameters into a map.
  let mut params: std::collections::HashMap<String, String> = std::collections::HashMap::new();
  for pair in query.split('&').filter(|p| !p.is_empty()) {
    if let Some((k, v)) = pair.split_once('=') {
      params.insert(percent_decode(k).to_ascii_lowercase(), percent_decode(v));
    }
  }

  let security: VlessSecurity = match params.get("security") {
    Some(s) => s.parse()?,
    None => VlessSecurity::Tls,
  };

  if let Some(enc) = params.get("encryption") {
    if enc != "none" {
      return Err(VpnError::InvalidVless(format!(
        "VLESS encryption must be 'none', got '{enc}'"
      )));
    }
  }

  let config = VlessConfig {
    address,
    port,
    uuid,
    security,
    flow: params.get("flow").cloned().unwrap_or_else(default_flow),
    fingerprint: params.get("fp").cloned().filter(|s| !s.is_empty()),
    server_name: params.get("sni").cloned().filter(|s| !s.is_empty()),
    public_key: params.get("pbk").cloned().filter(|s| !s.is_empty()),
    short_id: params.get("sid").cloned().filter(|s| !s.is_empty()),
    spider_x: params.get("spx").cloned().filter(|s| !s.is_empty()),
  };

  config.validate()?;
  Ok(config)
}

/// Serialize a `VlessConfig` back into a canonical `vless://` share link.
pub fn serve_vless_uri(config: &VlessConfig) -> Result<String, VpnError> {
  config.validate()?;
  let mut query = format!(
    "?encryption=none&security={}&flow={}",
    config.security.as_str(),
    config.flow
  );
  if let Some(fp) = &config.fingerprint {
    query.push_str(&format!("&fp={fp}"));
  }
  if let Some(sni) = &config.server_name {
    query.push_str(&format!("&sni={sni}"));
  }
  if let Some(pbk) = &config.public_key {
    query.push_str(&format!("&pbk={pbk}"));
  }
  if let Some(sid) = &config.short_id {
    query.push_str(&format!("&sid={sid}"));
  }
  if let Some(spx) = &config.spider_x {
    query.push_str(&format!("&spx={spx}"));
  }
  Ok(format!(
    "vless://{}@{}:{}{query}",
    config.uuid,
    format_host(&config.address),
    config.port
  ))
}

/// Extract the VLESS `vnext` entry and `streamSettings` from an outbound.
fn extract_outbound(
  outbound: &serde_json::Value,
) -> Result<(serde_json::Value, serde_json::Value), VpnError> {
  if outbound.get("protocol").and_then(|p| p.as_str()) != Some("vless") {
    return Err(VpnError::InvalidVless(
      "Outbound protocol is not vless".to_string(),
    ));
  }

  let vnext = outbound
    .get("settings")
    .and_then(|s| s.get("vnext"))
    .and_then(|v| v.as_array())
    .and_then(|a| a.first())
    .ok_or_else(|| {
      VpnError::InvalidVless("Missing settings.vnext in VLESS outbound".to_string())
    })?;

  let stream = outbound
    .get("streamSettings")
    .cloned()
    .unwrap_or_else(|| serde_json::json!({}));

  Ok((vnext.clone(), stream))
}

/// Parse a full Xray client JSON document (with an `outbounds` array) or a
/// bare VLESS outbound object into a `VlessConfig`.
pub fn parse_xray_config_json(content: &str) -> Result<VlessConfig, VpnError> {
  let value: serde_json::Value = serde_json::from_str(content)
    .map_err(|e| VpnError::InvalidVless(format!("Not valid JSON: {e}")))?;

  let outbound = match &value {
    serde_json::Value::Object(_) if value.get("protocol").is_some() => value.clone(),
    _ => value
      .get("outbounds")
      .and_then(|o| o.as_array())
      .and_then(|a| {
        a.iter()
          .find(|ob| ob.get("protocol").and_then(|p| p.as_str()) == Some("vless"))
      })
      .cloned()
      .ok_or_else(|| {
        VpnError::InvalidVless("No VLESS outbound found in Xray config".to_string())
      })?,
  };

  let (settings, stream) = extract_outbound(&outbound)?;

  let address = settings
    .get("address")
    .and_then(|a| a.as_str())
    .ok_or_else(|| VpnError::InvalidVless("Missing vnext.address".to_string()))?;
  let port = settings
    .get("port")
    .and_then(|p| p.as_u64())
    .ok_or_else(|| VpnError::InvalidVless("Missing vnext.port".to_string()))?;
  let user = settings
    .get("users")
    .and_then(|u| u.as_array())
    .and_then(|a| a.first())
    .ok_or_else(|| VpnError::InvalidVless("Missing vnext.users".to_string()))?;
  let uuid = user
    .get("id")
    .and_then(|i| i.as_str())
    .ok_or_else(|| VpnError::InvalidVless("Missing user.id".to_string()))?;

  let security_str = stream
    .get("security")
    .and_then(|s| s.as_str())
    .unwrap_or("tls");
  let security: VlessSecurity = security_str.parse()?;

  let (fingerprint, server_name, public_key, short_id, spider_x) = match security {
    VlessSecurity::Tls => (
      stream
        .get("tlsSettings")
        .and_then(|s| s.get("fingerprint"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string()),
      stream
        .get("tlsSettings")
        .and_then(|s| s.get("serverName"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string()),
      None,
      None,
      None,
    ),
    VlessSecurity::Reality => {
      let reality = || stream.get("realitySettings");
      (
        reality()
          .and_then(|s| s.get("fingerprint"))
          .and_then(|v| v.as_str())
          .map(|s| s.to_string()),
        reality()
          .and_then(|s| s.get("serverName"))
          .and_then(|v| v.as_str())
          .map(|s| s.to_string()),
        reality()
          .and_then(|s| s.get("publicKey"))
          .and_then(|v| v.as_str())
          .filter(|s| !s.is_empty())
          .map(|s| s.to_string()),
        reality()
          .and_then(|s| s.get("shortId"))
          .and_then(|v| v.as_str())
          .filter(|s| !s.is_empty())
          .map(|s| s.to_string()),
        reality()
          .and_then(|s| s.get("spiderId"))
          .or_else(|| reality().and_then(|s| s.get("spiderX")))
          .and_then(|v| v.as_str())
          .filter(|s| !s.is_empty())
          .map(|s| s.to_string()),
      )
    }
  };

  let config = VlessConfig {
    address: address.to_string(),
    port: port as u16,
    uuid: uuid.to_string(),
    security,
    flow: user
      .get("flow")
      .and_then(|f| f.as_str())
      .filter(|s| !s.is_empty())
      .map(|s| s.to_string())
      .unwrap_or_else(default_flow),
    fingerprint,
    server_name,
    public_key,
    short_id,
    spider_x,
  };

  config.validate()?;
  Ok(config)
}

/// Dispatch a raw content string (share link or Xray JSON) to a `VlessConfig`.
pub fn parse_vless_config(content: &str) -> Result<VlessConfig, VpnError> {
  let trimmed = content
    .trim()
    .strip_prefix('\u{feff}')
    .unwrap_or(content.trim());
  if trimmed.starts_with("vless://") {
    parse_vless_uri(trimmed)
  } else {
    parse_xray_config_json(trimmed)
  }
}

/// Render the Xray client config that `xray run -c` executes for a given
/// local SOCKS5 inbound port, feeding all traffic through the VLESS outbound.
pub fn vless_config_to_xray_client_json(
  config: &VlessConfig,
  local_socks_port: u16,
) -> Result<serde_json::Value, VpnError> {
  config.validate()?;

  let mut stream_settings = serde_json::json!({
    "network": "tcp",
    "security": config.security.as_str(),
  });

  match config.security {
    VlessSecurity::Reality => {
      let mut reality = serde_json::json!({
        "serverName": config.resolved_server_name(),
        "publicKey": config.public_key.as_deref().unwrap_or_default(),
        "shortId": config.short_id.as_deref().unwrap_or_default(),
      });
      if let Some(fp) = &config.fingerprint {
        reality["fingerprint"] = serde_json::Value::String(fp.clone());
      }
      if let Some(spx) = &config.spider_x {
        reality["spiderX"] = serde_json::Value::String(spx.clone());
      }
      stream_settings["realitySettings"] = reality;
    }
    VlessSecurity::Tls => {
      let mut tls = serde_json::json!({
        "serverName": config.resolved_server_name(),
        "allowInsecure": false,
      });
      if let Some(fp) = &config.fingerprint {
        tls["fingerprint"] = serde_json::Value::String(fp.clone());
      }
      stream_settings["tlsSettings"] = tls;
    }
  }

  let mut user = serde_json::json!({
    "id": config.uuid,
    "encryption": "none",
  });
  if !config.flow.is_empty() {
    user["flow"] = serde_json::Value::String(config.flow.clone());
  }

  let outbound = serde_json::json!({
    "tag": "proxy",
    "protocol": "vless",
    "settings": {
      "vnext": [
        {
          "address": config.address,
          "port": config.port,
          "users": [ user ],
        }
      ]
    },
    "streamSettings": stream_settings,
  });

  Ok(serde_json::json!({
    "log": { "loglevel": "warning" },
    "inbounds": [
      {
        "listen": "127.0.0.1",
        "port": local_socks_port,
        "protocol": "socks",
        "settings": { "udp": true },
        "sniffing": {
          "enabled": true,
          "destOverride": ["http", "tls", "quic"],
          "routeOnly": true
        }
      }
    ],
    "outbounds": [outbound]
  }))
}

/// Convenience: parse a `vless://…` URI and render the Xray client JSON.
pub fn vless_uri_to_xray_client_json(
  uri: &str,
  local_socks_port: u16,
) -> Result<serde_json::Value, VpnError> {
  let config = parse_vless_uri(uri)?;
  vless_config_to_xray_client_json(&config, local_socks_port)
}

#[cfg(test)]
mod tests {
  use super::*;

  const REALITY_URI: &str = "vless://0af941e8-9b48-4dd8-a953-2e9c91f31b3a@195.230.1.17:443?encryption=none&security=reality&fp=chrome&pbk=uz1jfVzZ04CZspLpNOrRPmv83a2X3pj37lq1yy0hA0A&sid=d9f3ff0ed2b26d77&flow=xtls-rprx-vision#VPN-Reality";

  #[test]
  fn test_parse_reality_uri() {
    let cfg = parse_vless_uri(REALITY_URI).unwrap();
    assert_eq!(cfg.address, "195.230.1.17");
    assert_eq!(cfg.port, 443);
    assert_eq!(cfg.uuid, "0af941e8-9b48-4dd8-a953-2e9c91f31b3a");
    assert_eq!(cfg.security, VlessSecurity::Reality);
    assert_eq!(cfg.flow, "xtls-rprx-vision");
    assert_eq!(cfg.fingerprint.as_deref(), Some("chrome"));
    assert_eq!(
      cfg.public_key.as_deref(),
      Some("uz1jfVzZ04CZspLpNOrRPmv83a2X3pj37lq1yy0hA0A")
    );
    assert_eq!(cfg.short_id.as_deref(), Some("d9f3ff0ed2b26d77"));
    assert_eq!(cfg.server_name, None);
    assert_eq!(cfg.spider_x, None);
  }

  #[test]
  fn test_parse_tls_uri() {
    let uri = "vless://7a5ec0f6-707d-4c3b-a220-44a16f9a4305@vpn.example.com:8443?encryption=none&security=tls&sni=vpn.example.com&fp=chrome&flow=xtls-rprx-vision&type=tcp";
    let cfg = parse_vless_uri(uri).unwrap();
    assert_eq!(cfg.address, "vpn.example.com");
    assert_eq!(cfg.port, 8443);
    assert_eq!(cfg.security, VlessSecurity::Tls);
    assert_eq!(cfg.server_name.as_deref(), Some("vpn.example.com"));
    assert_eq!(cfg.fingerprint.as_deref(), Some("chrome"));
    assert_eq!(cfg.flow, "xtls-rprx-vision");
  }

  #[test]
  fn test_parse_ipv6_uri() {
    let uri = "vless://5fd0aa4f-7ca0-4b67-b2f0-5f2d8cf6a1df@[::1]:443?encryption=none&security=tls";
    let cfg = parse_vless_uri(uri).unwrap();
    assert_eq!(cfg.address, "::1");
    assert_eq!(cfg.port, 443);
  }

  #[test]
  fn test_rejects_bad_encryption() {
    let uri = "vless://5fd0aa4f-7ca0-4b67-b2f0-5f2d8cf6a1df@example.com:443?encryption=aws";
    assert!(parse_vless_uri(uri).is_err());
  }

  #[test]
  fn test_validate_reality_requires_keys() {
    let cfg = VlessConfig {
      address: "server.example.com".to_string(),
      port: 443,
      uuid: "5fd0aa4f-7ca0-4b67-b2f0-5f2d8cf6a1df".to_string(),
      security: VlessSecurity::Reality,
      flow: "xtls-rprx-vision".to_string(),
      fingerprint: Some("chrome".to_string()),
      server_name: Some("server.example.com".to_string()),
      public_key: None,
      short_id: None,
      spider_x: None,
    };
    let err = cfg.validate().unwrap_err().to_string();
    assert!(err.contains("public key"));
  }

  #[test]
  fn test_parse_xray_config_reality() {
    let json = r#"{
      "log": { "loglevel": "warning" },
      "outbounds": [
        {
          "tag": "proxy",
          "protocol": "vless",
          "settings": {
            "vnext": [
              {
                "address": "vless.example.com",
                "port": 443,
                "users": [
                  { "id": "5fd0aa4f-7ca0-4b67-b2f0-5f2d8cf6a1df", "encryption": "none", "flow": "xtls-rprx-vision" }
                ]
              }
            ]
          },
          "streamSettings": {
            "network": "tcp",
            "security": "reality",
            "realitySettings": {
              "serverName": "vless.example.com",
              "fingerprint": "chrome",
              "publicKey": "co5xWpYNvPfwrNLxqB5QZHKLRsQY8Bydw9JmDcVXAvX",
              "shortId": "myba8516de3c30d16",
              "spiderId": "/"
            }
          }
        }
      ]
    }"#;
    let cfg = parse_xray_config_json(json).unwrap();
    assert_eq!(cfg.address, "vless.example.com");
    assert_eq!(cfg.port, 443);
    assert_eq!(cfg.security, VlessSecurity::Reality);
    assert_eq!(cfg.uuid, "5fd0aa4f-7ca0-4b67-b2f0-5f2d8cf6a1df");
    assert_eq!(
      cfg.public_key.as_deref(),
      Some("co5xWpYNvPfwrNLxqB5QZHKLRsQY8Bydw9JmDcVXAvX")
    );
  }

  #[test]
  fn test_vless_uri_to_xray_client_json_tls() {
    let uri = "vless://5fd0aa4f-7ca0-4b67-b2f0-5f2d8cf6a1df@vpn.example.com:443?encryption=none&security=tls&sni=vpn.example.com&flow=xtls-rprx-vision";
    let doc = vless_uri_to_xray_client_json(uri, 1080).unwrap();
    assert_eq!(doc["inbounds"][0]["protocol"], "socks");
    assert_eq!(doc["inbounds"][0]["port"], 1080);
    assert_eq!(doc["inbounds"][0]["listen"], "127.0.0.1");
    assert_eq!(doc["outbounds"][0]["protocol"], "vless");
    assert_eq!(
      doc["outbounds"][0]["settings"]["vnext"][0]["address"],
      "vpn.example.com"
    );
    assert_eq!(doc["outbounds"][0]["settings"]["vnext"][0]["port"], 443);
    assert_eq!(doc["outbounds"][0]["streamSettings"]["security"], "tls");
    assert_eq!(
      doc["outbounds"][0]["streamSettings"]["tlsSettings"]["serverName"],
      "vpn.example.com"
    );
    assert_eq!(
      doc["outbounds"][0]["settings"]["vnext"][0]["users"][0]["id"],
      "5fd0aa4f-7ca0-4b67-b2f0-5f2d8cf6a1df"
    );
    assert_eq!(
      doc["outbounds"][0]["settings"]["vnext"][0]["users"][0]["flow"],
      "xtls-rprx-vision"
    );
  }

  #[test]
  fn test_serve_vless_uri_roundtrip() {
    let cfg = parse_vless_uri(REALITY_URI).unwrap();
    let served = serve_vless_uri(&cfg).unwrap();
    let reparsed = parse_vless_uri(&served).unwrap();
    assert_eq!(reparsed.address, cfg.address);
    assert_eq!(reparsed.port, cfg.port);
    assert_eq!(reparsed.uuid, cfg.uuid);
    assert_eq!(reparsed.security, cfg.security);
    assert_eq!(reparsed.public_key, cfg.public_key);
    assert_eq!(reparsed.short_id, cfg.short_id);
  }

  #[test]
  fn test_percent_decoded_fragment_ignored() {
    let uri = "vless://5fd0aa4f-7ca0-4b67-b2f0-5f2d8cf6a1df@example.com:443?encryption=none&security=tls#My%20Server";
    let cfg = parse_vless_uri(uri).unwrap();
    assert_eq!(cfg.address, "example.com");
    assert_eq!(cfg.uuid, "5fd0aa4f-7ca0-4b67-b2f0-5f2d8cf6a1df");
  }
}
