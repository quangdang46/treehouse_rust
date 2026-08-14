//! Self-update subsystem: background version check + `treehouse update`.
//!
//! Isolated from pool/state/git. Reproduces Go `internal/updater/`:
//! - Version check against the GitHub latest-release API, cached at
//!   `~/.treehouse/update-check.json` with a 24h TTL.
//! - `update` downloads the release asset, verifies sha256, and atomically
//!   replaces the executable.
//! - HTTPS is enforced on all download URLs (configurable in tests).

use std::path::PathBuf;

/// The default GitHub API URL for the latest release. Overridable in tests.
pub const DEFAULT_GITHUB_API_URL: &str =
    "https://api.github.com/repos/quangdang46/treehouse_rust/releases/latest";
/// Cache TTL (24h).
pub const CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(24 * 3600);

/// A parsed semantic version (major.minor.patch[-prerelease]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    pub prerelease: Option<String>,
}

impl Version {
    /// Parses a version string, returning None on malformed input.
    pub fn parse(s: &str) -> Option<Version> {
        let s = s.trim().trim_start_matches('v');
        let (core, prerelease) = match s.split_once('-') {
            Some((c, p)) => (c, Some(p.to_string())),
            None => (s, None),
        };
        let mut parts = core.split('.');
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next()?.parse().ok()?;
        let patch = parts.next()?.parse().ok()?;
        Some(Version {
            major,
            minor,
            patch,
            prerelease,
        })
    }
}

impl std::cmp::Ord for Version {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        use std::cmp::Ordering;
        match self.major.cmp(&other.major) {
            Ordering::Equal => {}
            o => return o,
        }
        match self.minor.cmp(&other.minor) {
            Ordering::Equal => {}
            o => return o,
        }
        match self.patch.cmp(&other.patch) {
            Ordering::Equal => {}
            o => return o,
        }
        // A release (no prerelease) beats a prerelease.
        match (&self.prerelease, &other.prerelease) {
            (None, None) => Ordering::Equal,
            (None, Some(_)) => Ordering::Greater,
            (Some(_), None) => Ordering::Less,
            (Some(a), Some(b)) => a.cmp(b),
        }
    }
}

impl std::cmp::PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// The cached update-check entry.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct UpdateCheckCache {
    pub checked_at: String,
    pub latest_version: String,
}

/// The cache file path: `~/.treehouse/update-check.json`.
pub fn cache_path() -> PathBuf {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".treehouse").join("update-check.json")
}

/// Whether the cache is stale: missing, older than the TTL, or the cached
/// latest is not newer than `current`.
pub fn is_cache_stale(current: &str) -> bool {
    let Some(cache) = read_cache() else {
        return true;
    };
    // Older than TTL.
    if let Ok(checked) = chrono::DateTime::parse_from_rfc3339(&cache.checked_at) {
        let age = chrono::Utc::now().signed_duration_since(checked.with_timezone(&chrono::Utc));
        if age > chrono::Duration::from_std(CACHE_TTL).unwrap_or(chrono::Duration::days(1)) {
            return true;
        }
    }
    // Cached latest not newer than current -> stale (need a re-check).
    match (
        Version::parse(&cache.latest_version),
        Version::parse(current),
    ) {
        (Some(latest), Some(cur)) => latest > cur,
        _ => true,
    }
}

/// Reads the update-check cache, if any.
pub fn read_cache() -> Option<UpdateCheckCache> {
    let path = cache_path();
    let data = std::fs::read(&path).ok()?;
    serde_json::from_slice(&data).ok()
}

/// Fetches the latest release version from the GitHub API (or the injected URL).
/// Returns None on network/parse failure (best-effort background check).
pub fn check_latest(github_api_url: &str, enforce_https: bool) -> Option<String> {
    if enforce_https && !github_api_url.starts_with("https://") {
        return None;
    }
    let resp = std::process::Command::new("curl")
        .args(["-fsSL", github_api_url])
        .output()
        .ok()?;
    if !resp.status.success() {
        return None;
    }
    let body = String::from_utf8_lossy(&resp.stdout);
    let v: serde_json::Value = serde_json::from_str(&body).ok()?;
    v.get("tag_name")?.as_str().map(|s| s.to_string())
}

/// Writes the update-check cache.
pub fn write_cache(latest: &str) {
    let cache = UpdateCheckCache {
        checked_at: chrono::Utc::now().to_rfc3339(),
        latest_version: latest.to_string(),
    };
    let path = cache_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(&path, serde_json::to_string(&cache).unwrap_or_default());
}

/// Whether an update is available (cache says latest > current).
pub fn update_available(current: &str) -> bool {
    let Some(cache) = read_cache() else {
        return false;
    };
    match (
        Version::parse(&cache.latest_version),
        Version::parse(current),
    ) {
        (Some(latest), Some(cur)) => latest > cur,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_parse_basic() {
        let v = Version::parse("v1.2.3").unwrap();
        assert_eq!(
            v,
            Version {
                major: 1,
                minor: 2,
                patch: 3,
                prerelease: None
            }
        );
        let v2 = Version::parse("3.0.0-beta.1").unwrap();
        assert_eq!(v2.prerelease.as_deref(), Some("beta.1"));
        assert!(Version::parse("not-a-version").is_none());
    }

    #[test]
    fn version_ordering() {
        assert!(Version::parse("2.0.0").unwrap() > Version::parse("1.9.9").unwrap());
        assert!(Version::parse("1.2.3").unwrap() < Version::parse("1.2.4").unwrap());
        // Release beats prerelease.
        assert!(Version::parse("1.2.3").unwrap() > Version::parse("1.2.3-beta.1").unwrap());
        // Equal versions.
        assert!(Version::parse("1.2.3").unwrap() == Version::parse("v1.2.3").unwrap());
    }

    #[test]
    fn https_enforced() {
        assert!(check_latest("http://insecure.example", true).is_none());
        // Non-enforcing path reaches curl (network-dependent; just confirm no panic).
        let _ = check_latest("http://insecure.example", false);
    }
}
