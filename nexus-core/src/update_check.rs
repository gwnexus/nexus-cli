//! Fast, cached update check for the Nexus CLI.
//!
//! On startup the CLI can call [`check_for_update`] which:
//!   1. Reads a local cache file (`~/.config/nexus/update-check.json`).
//!   2. If the cache is fresh (< 24 h) it returns the cached result instantly.
//!   3. Otherwise it fetches the latest release tag from GitHub (with a 3 s
//!      connect timeout) and updates the cache.
//!
//! The caller is responsible for printing the result; this module never blocks
//! longer than 3 seconds on the network.

use crate::config::Config;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// How long the cached version info is considered fresh.
const CACHE_TTL_SECS: u64 = 24 * 60 * 60; // 24 hours

/// GitHub API endpoint for the latest release.
const GITHUB_LATEST: &str = "https://api.github.com/repos/gwnexus/nexus-cli/releases/latest";

/// Network timeout for the GitHub API call.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(3);

/// Cached update-check state persisted to disk.
#[derive(Debug, Serialize, Deserialize)]
struct Cache {
    /// Latest version tag (without leading 'v').
    latest_version: String,
    /// Unix timestamp of the last successful check.
    checked_at: u64,
}

/// Result of an update check.
#[derive(Debug)]
pub struct UpdateInfo {
    /// Currently running version.
    pub current: String,
    /// Latest available version (if known).
    pub latest: String,
    /// Whether an update is available.
    pub update_available: bool,
}

fn cache_path() -> Option<PathBuf> {
    Config::dir().ok().map(|d| d.join("update-check.json"))
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn load_cache() -> Option<Cache> {
    let path = cache_path()?;
    let data = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&data).ok()
}

fn save_cache(cache: &Cache) {
    if let Some(path) = cache_path() {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(&path, serde_json::to_string(cache).unwrap_or_default());
    }
}

/// Minimal GitHub release response — we only need the tag name.
#[derive(Deserialize)]
struct GhRelease {
    tag_name: String,
}

/// Fetch the latest version tag from GitHub.
async fn fetch_latest() -> Option<String> {
    let client = reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .connect_timeout(REQUEST_TIMEOUT)
        .user_agent(format!("nexus-cli/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .ok()?;

    let resp = client.get(GITHUB_LATEST).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let release: GhRelease = resp.json().await.ok()?;
    Some(release.tag_name.trim_start_matches('v').to_string())
}

/// Simple semver comparison (major.minor.patch).
/// Returns true if `latest` is strictly newer than `current`.
fn is_newer(current: &str, latest: &str) -> bool {
    let parse = |s: &str| -> (u64, u64, u64) {
        let mut parts = s.split('.').map(|p| p.parse::<u64>().unwrap_or(0));
        (
            parts.next().unwrap_or(0),
            parts.next().unwrap_or(0),
            parts.next().unwrap_or(0),
        )
    };
    let c = parse(current);
    let l = parse(latest);
    l > c
}

/// Check for a newer CLI version.
///
/// - Returns `None` if updates are disabled, the check fails, or the cache is
///   fresh and no update is available.
/// - Never blocks longer than [`REQUEST_TIMEOUT`].
pub async fn check_for_update(config: &crate::config::Config) -> Option<UpdateInfo> {
    if !config.check_updates {
        return None;
    }

    let current = env!("CARGO_PKG_VERSION").to_string();

    // Try cache first
    if let Some(cache) = load_cache() {
        let age = now_secs().saturating_sub(cache.checked_at);
        if age < CACHE_TTL_SECS {
            // Cache is fresh — return result from cache
            if is_newer(&current, &cache.latest_version) {
                return Some(UpdateInfo {
                    current,
                    latest: cache.latest_version,
                    update_available: true,
                });
            }
            return None; // up to date, no need to nag
        }
    }

    // Cache stale or missing — fetch from GitHub
    let latest = fetch_latest().await?;

    // Persist the result
    save_cache(&Cache {
        latest_version: latest.clone(),
        checked_at: now_secs(),
    });

    if is_newer(&current, &latest) {
        Some(UpdateInfo {
            current,
            latest,
            update_available: true,
        })
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_newer() {
        assert!(is_newer("0.6.11", "0.6.12"));
        assert!(is_newer("0.6.12", "0.7.0"));
        assert!(is_newer("0.6.12", "1.0.0"));
        assert!(!is_newer("0.6.12", "0.6.12"));
        assert!(!is_newer("0.6.12", "0.6.11"));
        assert!(!is_newer("1.0.0", "0.9.99"));
    }

    #[test]
    fn test_cache_roundtrip() {
        let cache = Cache {
            latest_version: "0.7.0".to_string(),
            checked_at: now_secs(),
        };
        let json = serde_json::to_string(&cache).unwrap();
        let parsed: Cache = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.latest_version, "0.7.0");
    }

    #[tokio::test]
    async fn test_check_for_update_disabled() {
        let mut config = crate::config::Config::default();
        config.check_updates = false;
        let result = check_for_update(&config).await;
        assert!(result.is_none());
    }

    #[test]
    fn test_is_newer_edge_cases() {
        // Same version
        assert!(!is_newer("1.0.0", "1.0.0"));
        // Partial versions
        assert!(is_newer("0.6", "0.7"));
        assert!(!is_newer("1", "0"));
    }
}
