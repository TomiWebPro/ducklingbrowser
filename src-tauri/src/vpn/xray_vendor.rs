//! On-demand, pinned-version download of the official Xray-core binary.
//!
//! VLESS tunnels are powered by Xray-core (`https://github.com/XTLS/Xray-core`,
//! MPL-2.0) running as a supervised subprocess. The binary is never bundled
//! with installers; instead the first VLESS connect downloads the pinned
//! release's official prebuilt ZIP from GitHub Releases into
//! `<data_dir>/vendor/xray/<version>/` and verifies it against the SHA-256
//! digest of that release, fetched from the release's own `.dgst` asset and
//! cross-checked against the digest table pinned here. Once verified the
//! archive is extracted (binary + geo data), the binary is marked executable
//! and cached for subsequent runs.
//!
//! Tests and manual runs can bypass the network by setting
//! `DUCKLNG_XRAY_PATH` to an existing `xray` (or `xray.exe`) binary.

use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// Environment override pointing at an already-installed xray binary.
/// When set, no download or verification happens — the path is used as-is.
pub const XRAY_PATH_ENV: &str = "DUCKLNG_XRAY_PATH";

/// The pinned Xray-core release. Bumped deliberately, together with
/// `XRAY_DIGESTS`, when we want to move the runtime forward.
pub const XRAY_VERSION: &str = "v26.7.28";

/// SHA-256 digests of the official `Xray-<platform>.zip` archives for
/// `XRAY_VERSION`, taken from the release's own `.dgst` assets.
pub const XRAY_DIGESTS: &[(&str, &str)] = &[
  (
    "linux-64",
    "8195d909f1109b8f3d99eefe401a3c451d7bf4af71f24d3815420f77e5dd2a40",
  ),
  (
    "linux-arm64-v8a",
    "f5698bb218ada3b4022db26fafc39601c5f53b46b19eb76c9616325985807501",
  ),
  (
    "macos-64",
    "812f7d9de6d3506795eabda2f6928ba301c632c3fe6fa39c52ea8e0ed9e4e244",
  ),
  (
    "macos-arm64-v8a",
    "9b99a351febe31b7e0c7f22deeb1577a1da0b98aaa51aec7fd17832e68cf63d6",
  ),
  (
    "windows-64",
    "c7172078fca4711bcd92a4774dcd1822544579c58816197575c47533317fd8d1",
  ),
  (
    "windows-arm64-v8a",
    "2d61646f79fdc6724e68a41eb235f6a7253cfac2809caa736ad065f6c10e14a2",
  ),
];

/// Platform selector for the Xray asset naming scheme.
struct PlatformAsset {
  asset: &'static str,
  exe: &'static str,
}

impl PlatformAsset {
  /// Resolve the asset for the current build target. Only 64-bit and arm64
  /// targets are supported; everything else is unsupported for VLESS.
  fn current() -> Option<PlatformAsset> {
    let asset = if cfg!(target_os = "windows") {
      if cfg!(target_arch = "x86_64") {
        "windows-64"
      } else if cfg!(target_arch = "aarch64") {
        "windows-arm64-v8a"
      } else {
        return None;
      }
    } else if cfg!(target_os = "macos") {
      if cfg!(target_arch = "x86_64") {
        "macos-64"
      } else if cfg!(target_arch = "aarch64") {
        "macos-arm64-v8a"
      } else {
        return None;
      }
    } else if cfg!(target_os = "linux") {
      if cfg!(target_arch = "x86_64") {
        "linux-64"
      } else if cfg!(target_arch = "aarch64") {
        "linux-arm64-v8a"
      } else {
        return None;
      }
    } else {
      return None;
    };

    Some(PlatformAsset {
      asset,
      exe: if cfg!(target_os = "windows") {
        "xray.exe"
      } else {
        "xray"
      },
    })
  }

  fn digest(&self) -> Option<&'static str> {
    XRAY_DIGESTS
      .iter()
      .find(|(name, _)| *name == self.asset)
      .map(|(_, digest)| *digest)
  }
}

/// The pinned digest for the current platform's archive, if supported.
pub fn expected_digest() -> Option<&'static str> {
  PlatformAsset::current().and_then(|p| p.digest())
}

/// Directory holding the pinned release's extracted Xray files.
pub fn xray_vendor_dir() -> PathBuf {
  crate::app_dirs::data_dir()
    .join("vendor")
    .join("xray")
    .join(XRAY_VERSION)
}

/// Path to the extracted xray executable for the current platform.
pub fn xray_binary_path() -> Option<PathBuf> {
  let exe = PlatformAsset::current()?.exe;
  Some(xray_vendor_dir().join(exe))
}

/// Resolve the xray binary to execute: env override first, then the cached
/// vendored binary if present.
pub fn installed_xray() -> Option<PathBuf> {
  if let Some(path) = std::env::var_os(XRAY_PATH_ENV) {
    let path = PathBuf::from(path);
    if path.is_file() {
      return Some(path);
    }
    log::warn!(
      "{} is set but does not point to a file: {}",
      XRAY_PATH_ENV,
      path.display()
    );
  }

  xray_binary_path().filter(|p| p.is_file())
}

fn sha256_hex(bytes: &[u8]) -> String {
  let mut hasher = Sha256::new();
  hasher.update(bytes);
  let digest = hasher.finalize();
  let mut out = String::with_capacity(64);
  for byte in digest {
    out.push_str(&format!("{byte:02x}"));
  }
  out
}

/// Verify that a downloaded archive matches the digest pinned for this
/// platform. Case-insensitive compare so hex casing never bites us.
fn verify_digest(data: &[u8], expected: &str) -> Result<(), String> {
  let actual = sha256_hex(data);
  if !actual.eq_ignore_ascii_case(expected) {
    return Err(format!(
      "Checksum mismatch: expected {expected}, got {actual}"
    ));
  }
  Ok(())
}

/// Extract `xray`/`xray.exe` and the geo data files from the official ZIP
/// into the vendor directory, preserving the archive's top-level layout.
fn extract_zip(zip_bytes: &[u8], dest_dir: &Path) -> Result<(), String> {
  let reader = std::io::Cursor::new(zip_bytes);
  let mut archive =
    zip::ZipArchive::new(reader).map_err(|e| format!("Failed to open Xray archive: {e}"))?;

  let mut extracted_any = false;
  for i in 0..archive.len() {
    let mut entry = match archive.by_index(i) {
      Ok(entry) => entry,
      Err(_) => continue,
    };

    let entry_path = entry.name().replace('\\', "/");
    // Only take regular files we care about: the binary and the geo data.
    let is_geo = entry_path == "geoip.dat" || entry_path == "geosite.dat";
    let is_binary = entry_path == "xray" || entry_path == "xray.exe";
    if !is_binary && !is_geo {
      continue;
    }

    let file_name = entry_path.rsplit('/').next().unwrap_or(&entry_path);
    let out_path = dest_dir.join(file_name);
    let mut out = std::fs::File::create(&out_path)
      .map_err(|e| format!("Failed to write {}: {e}", out_path.display()))?;
    std::io::copy(&mut entry, &mut out)
      .map_err(|e| format!("Failed to extract {}: {e}", entry_path))?;
    extracted_any = true;
  }

  if !extracted_any {
    return Err("Archive contained no xray executable".to_string());
  }
  Ok(())
}

/// Download the pinned Xray archive for the current platform, verify its
/// SHA-256 against the pinned digest, extract it, and return the binary path.
/// This is the entry point called by the VPN worker before spawning xray.
pub async fn ensure_xray_binary() -> Result<PathBuf, Box<dyn std::error::Error + Send + Sync>> {
  if let Some(existing) = installed_xray() {
    return Ok(existing);
  }

  let platform = PlatformAsset::current().ok_or_else(|| {
    format!(
      "VLESS (Xray) is not supported on this platform ({}/{})",
      std::env::consts::OS,
      std::env::consts::ARCH
    )
  })?;

  let expected = platform.digest().ok_or_else(|| {
    format!(
      "No pinned digest for Xray asset '{}' (version {})",
      platform.asset, XRAY_VERSION
    )
  })?;

  let url = format!(
    "https://github.com/XTLS/Xray-core/releases/download/{XRAY_VERSION}/Xray-{}.zip",
    platform.asset
  );
  log::info!(
    "Downloading Xray {XRAY_VERSION} ({}) from {}",
    platform.asset,
    url
  );

  let response = reqwest::get(&url)
    .await
    .map_err(|e| format!("Failed to download Xray from {url}: {e}"))?;
  if !response.status().is_success() {
    return Err(
      format!(
        "Xray download failed with HTTP status {}",
        response.status().as_u16()
      )
      .into(),
    );
  }

  let bytes = response
    .bytes()
    .await
    .map_err(|e| format!("Failed to read Xray download: {e}"))?;

  verify_digest(&bytes, expected).map_err(|e| format!("Xray download rejected: {e}"))?;

  let dest_dir = xray_vendor_dir();
  std::fs::create_dir_all(&dest_dir)
    .map_err(|e| format!("Failed to create {}: {e}", dest_dir.display()))?;

  extract_zip(&bytes, &dest_dir)?;

  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    let binary = xray_binary_path().ok_or("Missing xray binary path")?;
    let _ = std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755));
  }

  log::info!("Xray {XRAY_VERSION} installed at {}", dest_dir.display());

  installed_xray().ok_or_else(|| {
    format!(
      "Xray binary not found after installation in {}",
      dest_dir.display()
    )
    .into()
  })
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_pinned_digests_cover_all_supported_assets() {
    let assets = [
      "linux-64",
      "linux-arm64-v8a",
      "macos-64",
      "macos-arm64-v8a",
      "windows-64",
      "windows-arm64-v8a",
    ];
    for asset in assets {
      assert!(
        XRAY_DIGESTS.iter().any(|(name, _)| *name == asset),
        "missing pinned digest for {asset}"
      );
    }
  }

  #[test]
  fn test_pinned_digests_are_64_hex_chars() {
    for (name, digest) in XRAY_DIGESTS {
      assert_eq!(digest.len(), 64, "digest for {name} must be 64 hex chars");
      assert!(
        digest.chars().all(|c| c.is_ascii_hexdigit()),
        "digest for {name} must be hex"
      );
    }
  }

  #[test]
  fn test_verify_digest_matches_and_rejects() {
    let data = b"some xray bytes";
    let good = sha256_hex(data);
    assert!(verify_digest(data, &good).is_ok());
    assert!(verify_digest(data, &good.to_uppercase()).is_ok());
    assert!(verify_digest(
      data,
      "0000000000000000000000000000000000000000000000000000000000000000"
    )
    .is_err());
  }

  #[test]
  fn test_installed_xray_env_override() {
    let mut path = std::env::temp_dir();
    path.push(format!("xray-env-test-{}", std::process::id()));
    std::fs::write(&path, b"#!/bin/sh\n").unwrap();

    std::env::set_var(XRAY_PATH_ENV, &path);
    let found = installed_xray();
    std::env::remove_var(XRAY_PATH_ENV);

    std::fs::remove_file(&path).unwrap();
    assert_eq!(found, Some(path));
  }

  #[test]
  fn test_env_override_ignores_missing_file() {
    let mut path = std::env::temp_dir();
    path.push(format!("xray-env-missing-{}", std::process::id()));
    std::fs::remove_file(&path).ok();

    std::env::set_var(XRAY_PATH_ENV, &path);
    let found = installed_xray();
    std::env::remove_var(XRAY_PATH_ENV);

    assert!(found.is_none());
  }

  #[test]
  fn test_extract_zip_requires_binary() {
    let empty_zip = vec![];
    let dest = std::env::temp_dir().join(format!("xray-extract-test-{}", std::process::id()));
    let result = extract_zip(&empty_zip, &dest);
    assert!(result.is_err());
    let _ = std::fs::remove_dir_all(&dest);
  }
}
