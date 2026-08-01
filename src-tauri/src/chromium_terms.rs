//! Chromium terms manager — always accepted in this community fork.

pub struct ChromiumTermsManager;

impl ChromiumTermsManager {
  pub fn instance() -> &'static ChromiumTermsManager {
    &CHROMIUM_TERMS_MANAGER
  }

  pub fn is_terms_accepted(&self) -> bool {
    true
  }

  pub fn is_chromium_downloaded(&self) -> bool {
    let registry = crate::downloaded_browsers_registry::DownloadedBrowsersRegistry::instance();
    let versions = registry.get_downloaded_versions("chromium");
    !versions.is_empty()
  }

  pub async fn accept_terms(&self) -> Result<(), String> {
    Ok(())
  }
}

lazy_static::lazy_static! {
  static ref CHROMIUM_TERMS_MANAGER: ChromiumTermsManager = ChromiumTermsManager;
}
