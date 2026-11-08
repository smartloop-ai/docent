//! Whether a newer docent is out: the latest GitHub release, read off the
//! `/releases/latest` redirect as install.sh reads it, so no API token and
//! no rate limit are involved.

use std::time::Duration;

const REPO: &str = "smartloop-ai/docent";

/// The command that installs the latest release over this one.
pub fn upgrade_command() -> &'static str {
    if cfg!(windows) { "irm https://smartloop.ai/install.ps1 | iex" } else { "curl -fsSL https://smartloop.ai/install | sh" }
}

/// The latest release's version when it is newer than this build; `None`
/// when this is the latest, or GitHub can't be reached.
pub fn newer_release() -> Option<String> {
    latest_release().filter(|latest| is_newer(latest, env!("CARGO_PKG_VERSION")))
}

fn latest_release() -> Option<String> {
    // A client of its own: the app's carries the platform token, which is
    // not GitHub's to see.
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(5))
        .user_agent(concat!("docent/", env!("CARGO_PKG_VERSION")))
        .build()
        .ok()?;
    let response = client.head(format!("https://github.com/{}/releases/latest", REPO)).send().ok()?;
    version_from_url(response.url().as_str())
}

/// `…/releases/tag/v1.2.3` → `1.2.3`. A repo with no releases redirects to
/// `/releases` instead, which is no version.
fn version_from_url(url: &str) -> Option<String> {
    let (_, tag) = url.rsplit_once("/tag/")?;
    let version = tag.strip_prefix('v').unwrap_or(tag);
    parse(version).map(|_| version.to_string())
}

fn parse(version: &str) -> Option<Vec<u64>> {
    version.split('.').map(|part| part.parse().ok()).collect()
}

fn is_newer(latest: &str, current: &str) -> bool {
    matches!((parse(latest), parse(current)), (Some(l), Some(c)) if l > c)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_version_off_the_release_url() {
        assert_eq!(version_from_url("https://github.com/smartloop-ai/docent/releases/tag/v1.0.58").as_deref(), Some("1.0.58"));
        assert_eq!(version_from_url("https://github.com/smartloop-ai/docent/releases"), None);
        assert_eq!(version_from_url("https://github.com/smartloop-ai/docent/releases/tag/nightly"), None);
    }

    #[test]
    fn compares_versions_numerically() {
        assert!(is_newer("1.0.58", "1.0.57"));
        assert!(is_newer("1.0.100", "1.0.99"));
        assert!(is_newer("1.1.0", "1.0.99"));
        assert!(!is_newer("1.0.57", "1.0.57"));
        assert!(!is_newer("1.0.56", "1.0.57"));
    }
}
