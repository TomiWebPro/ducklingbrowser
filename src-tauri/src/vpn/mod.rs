//! VPN support module for WireGuard and VLESS configurations.
//!
//! This module provides:
//! - WireGuard config parsing (`.conf` files)
//! - VLESS (Reality/TLS) config parsing (`vless://` share links and Xray JSON)
//! - Encrypted storage for VPN configurations
//! - Tunnel management with userspace WireGuard (boringtun) routed through smoltcp

mod config;
pub mod socks5_server;
mod storage;
mod tunnel;
mod wireguard;
pub mod xray;

pub use config::{
  detect_vpn_type, parse_wireguard_config, VpnConfig, VpnError, VpnImportResult, VpnStatus,
  VpnType, WireGuardConfig,
};
pub use storage::VpnStorage;
pub use tunnel::{TunnelManager, VpnTunnel};
pub use wireguard::WireGuardTunnel;
pub use xray::{
  parse_vless_config, parse_vless_uri, parse_xray_config_json, serve_vless_uri,
  vless_config_to_xray_client_json, vless_uri_to_xray_client_json, VlessConfig, VlessSecurity,
};

use once_cell::sync::Lazy;
use std::sync::Mutex;

/// Global VPN storage instance
pub static VPN_STORAGE: Lazy<Mutex<VpnStorage>> = Lazy::new(|| Mutex::new(VpnStorage::new()));
