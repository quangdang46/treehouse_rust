//! Self-update subsystem: background version check + `treehouse update`.
//!
//! Isolated from pool/state/git. Reproduces Go `internal/updater/`:
//! - Version check against the GitHub latest-release API, cached at
//!   `~/.treehouse/update-check.json` with a 24h TTL.
//! - `update` downloads the release asset, verifies sha256, and atomically
//!   replaces the executable.
//! - HTTPS is enforced on all download URLs (configurable in tests).
//!
//! The apply half ([`apply`]) is the part that actually changes bytes on disk:
//! download → verify sha256 against the release sidecar → extract → de-quarantine
//! → temp-file-in-target-dir + rename. Every step returns `Err`; nothing here
//! can report success for an update that did not happen.

use std::path::{Path, PathBuf};

use crate::env::TreehouseEnv;

/// Reads the mode bits out of a `Permissions` value. On unix that is the whole
/// struct; on Windows there are no mode bits, so the original value is passed
/// straight back to `set_permissions` and this is the identity.
#[cfg(unix)]
fn mode_of(p: std::fs::Permissions) -> std::fs::Permissions {
    use std::os::unix::fs::PermissionsExt;
    std::fs::Permissions::from_mode(p.mode())
}

#[cfg(not(unix))]
fn mode_of(p: std::fs::Permissions) -> std::fs::Permissions {
    p
}

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
///
/// This is the *cache-write* half of the check. The download half is
/// [`check_latest_result`] + [`apply`]; use those for `treehouse update`, which
/// must not report success without having replaced the binary.
pub fn check_latest(github_api_url: &str, enforce_https: bool) -> Option<String> {
    let body = curl_capture(github_api_url, enforce_https, MAX_API_RESPONSE_SIZE).ok()?;
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

// ─── _with_env variants ────────────────────────────────────────────────────────

/// The cache file path using the injected environment.
pub fn cache_path_with_env(env: &dyn TreehouseEnv) -> Option<PathBuf> {
    env.update_cache_path()
}

/// Whether the cache is stale using the injected environment.
pub fn is_cache_stale_with_env(current: &str, env: &dyn TreehouseEnv) -> bool {
    let Some(cache) = read_cache_with_env(env) else {
        return true;
    };
    // Older than TTL.
    if let Ok(checked) = chrono::DateTime::parse_from_rfc3339(&cache.checked_at) {
        let age = chrono::Utc::now().signed_duration_since(checked.with_timezone(&chrono::Utc));
        if age > chrono::Duration::from_std(CACHE_TTL).unwrap_or(chrono::Duration::days(1)) {
            return true;
        }
    }
    // Cached latest not newer than current -> stale.
    match (
        Version::parse(&cache.latest_version),
        Version::parse(current),
    ) {
        (Some(latest), Some(cur)) => latest > cur,
        _ => true,
    }
}

/// Reads the update-check cache using the injected environment.
pub fn read_cache_with_env(env: &dyn TreehouseEnv) -> Option<UpdateCheckCache> {
    let path = env.update_cache_path()?;
    let data = env.read_bytes(&path).ok()?;
    serde_json::from_slice(&data).ok()
}

/// Writes the update-check cache using the injected environment.
pub fn write_cache_with_env(env: &dyn TreehouseEnv, latest: &str) {
    let Some(path) = env.update_cache_path() else {
        return;
    };
    let cache = UpdateCheckCache {
        checked_at: chrono::Utc::now().to_rfc3339(),
        latest_version: latest.to_string(),
    };
    if let Some(dir) = path.parent() {
        let _ = env.ensure_dir(dir);
    }
    let _ = env.write_file(
        &path,
        serde_json::to_string(&cache).unwrap_or_default().as_bytes(),
    );
}

/// Whether an update is available using the injected environment.
pub fn update_available_with_env(current: &str, env: &dyn TreehouseEnv) -> bool {
    let Some(cache) = read_cache_with_env(env) else {
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

// ─── Apply pipeline ───────────────────────────────────────────────────────────
//
// Go's `internal/updater` does this in `Apply` (updater.go:319). The port keeps
// the same five steps and, more importantly, the same failure discipline: a
// step that does not fully succeed returns Err and the binary on disk is left
// exactly as it was. The whole reason M-027 was filed is that the old CLI
// printed "Successfully updated" for an update that never happened, so nothing
// below is allowed to return Ok on a partial result.

// ─── Limits ────────────────────────────────────────────────────────────────────

/// Ceiling on a downloaded release archive (Go `maxDownloadSize`). A release
/// asset is tens of MB, so 100 MB is generous; past it we are either looking at
/// a broken release or at something trying to fill the user's disk.
const MAX_DOWNLOAD_SIZE: u64 = 100 << 20;

/// Ceiling on the *extracted* binary, kept separate from the archive cap
/// because a small archive can expand without bound — this is the number that
/// bounds what actually gets written toward the target directory.
const MAX_BINARY_SIZE: u64 = 100 << 20;

/// Ceiling on the GitHub API response (Go `maxAPIResponseSize`).
const MAX_API_RESPONSE_SIZE: u64 = 5 << 20;

/// Ceiling on the checksum sidecar: one 64-hex line per release asset, a few
/// KB at most. Anything bigger is not a checksum file.
const MAX_CHECKSUM_SIZE: u64 = 1 << 20;

/// Curl timeout for the API and checksum fetches (Go `httpTimeout` = 30s).
const FETCH_TIMEOUT_SECS: &str = "30";

/// Curl timeout for the asset download. Go allows 5 minutes here for the same
/// reason: this is the one big transfer.
const DOWNLOAD_TIMEOUT_SECS: &str = "300";

/// Prefix every release asset starts with (`.github/workflows/release.yml`).
const ASSET_PREFIX: &str = "treehouse-v";

/// Per-asset checksum sidecar published next to every release asset
/// (release.yml:94,99). Note that the workflow strips the container extension
/// before appending it — see [`checksum_sidecar_names`], which accepts both that
/// spelling and the extension-retained one.
const CHECKSUM_SUFFIX: &str = ".sha256";

/// Go's aggregate checksum manifest (`updater.go:29`). The Rust release
/// workflow emits per-asset sidecars instead, but both are the same
/// `sha256  filename` line format, so [`checksum_for`] reads either.
const AGGREGATE_CHECKSUM_FILE: &str = "checksums.txt";

/// The archive extension this platform's asset uses — zip on Windows, tar.gz
/// everywhere else (Go `extractBinary`, updater.go:573-578).
const fn archive_ext() -> &'static str {
    if cfg!(windows) { "zip" } else { "tar.gz" }
}

/// The binary's file name inside the archive, per platform (Go `extractTarGz`
/// and `extractZip` hard-code these).
const fn binary_name() -> &'static str {
    if cfg!(windows) {
        "treehouse.exe"
    } else {
        "treehouse"
    }
}

// ─── Errors ───────────────────────────────────────────────────────────────────

/// Why an update did not happen. Every variant is a path where the binary on
/// disk is unchanged; there is deliberately no variant meaning "partially
/// applied", because no code path produces one.
#[derive(Debug, thiserror::Error)]
pub enum UpdateError {
    #[error("no download URL for {os}/{arch}")]
    NoDownloadUrl {
        os: &'static str,
        arch: &'static str,
    },

    /// HTTPS rejection. The caller prefixes this with the URL's role
    /// ("download URL: " / "checksum URL: ") so the reason reads on its own.
    #[error("{0}")]
    InsecureUrl(String),

    #[error("fetching latest release: {0}")]
    Fetch(String),

    #[error("downloading update: {0}")]
    Download(String),

    #[error("checksum verification failed: {0}")]
    Checksum(String),

    #[error("extracting update: {0}")]
    Extract(String),

    #[error("resolving current executable: {0}")]
    ResolveExecutable(String),

    #[error("replacing binary: {0}")]
    Replace(String),
}

// ─── Release metadata ─────────────────────────────────────────────────────────

/// The outcome of a version check: what the latest release is, and — when one
/// exists for this platform — where to fetch it and how to verify it.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct CheckResult {
    pub current_version: String,
    pub latest_version: String,
    pub update_available: bool,
    /// The release archive for this OS/arch, if the release ships one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asset_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub download_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checksum_url: Option<String>,
}

/// What [`apply`] actually did. The CLI should print its success line from this
/// and not before, so "updated X -> Y" can only be said about a binary that was
/// really replaced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Applied {
    pub from_version: String,
    pub to_version: String,
    /// The path that was replaced, after symlink resolution.
    pub replaced_path: PathBuf,
}

/// The subset of the GitHub latest-release response we consume
/// (Go `githubRelease`).
#[derive(Debug, serde::Deserialize)]
struct GithubRelease {
    #[serde(rename = "tag_name", default)]
    tag_name: String,
    #[serde(default)]
    assets: Vec<GithubAsset>,
}

#[derive(Debug, serde::Deserialize)]
struct GithubAsset {
    name: String,
    #[serde(rename = "browser_download_url")]
    browser_download_url: String,
}

// ─── Asset selection ──────────────────────────────────────────────────────────

/// Whether `name` is this platform's release asset, regardless of version.
///
/// The suffix vocabulary is the release matrix in `.github/workflows/release.yml`
/// (`linux-x86_64`, `macos-aarch64`, `windows-x86_64`, …), NOT Go's
/// GOOS/GOARCH spelling — the Rust workflow publishes `macos-aarch64`, so
/// copying Go's matcher verbatim would find nothing on Apple Silicon.
fn matches_asset(name: &str, os: &str, arch: &str, ext: &str) -> bool {
    name.starts_with(ASSET_PREFIX) && name.ends_with(&format!("-{os}-{arch}.{ext}"))
}

/// [`matches_asset`] bound to the running platform. Used by both the release
/// lookup and the checksum sidecar lookup, so the two can never disagree about
/// which file is "the current platform's asset".
pub fn matches_current_platform_asset(name: &str) -> bool {
    matches_asset(
        name,
        std::env::consts::OS,
        std::env::consts::ARCH,
        archive_ext(),
    )
}

/// Container extensions a release asset can carry.
///
/// Only `.tar.gz` and `.zip` are ever written by `release.yml`; the rest are
/// here so a hand-uploaded asset does not silently lose its sidecar match just
/// because its container is spelled differently. Ordered longest-first: `.gz`
/// would otherwise strip the wrong end of `.tar.gz`.
const ARCHIVE_EXTS: &[&str] = &[".tar.gz", ".tar.xz", ".tgz", ".txz", ".zip", ".gz", ".xz"];

/// `name` without its container extension, or `None` if it has none.
///
/// The length guard keeps a bare `.gz` (or a name that is nothing but an
/// extension) from stripping to an empty stem, which would make every
/// `<stem>.sha256` name match.
fn strip_archive_ext(name: &str) -> Option<&str> {
    ARCHIVE_EXTS
        .iter()
        .find(|ext| name.len() > ext.len() && name.ends_with(**ext))
        .map(|ext| &name[..name.len() - ext.len()])
}

/// Every sidecar name that could carry the hash for `archive_name`, most
/// specific first.
///
/// Two spellings exist in the wild and both have to resolve:
///
/// - `treehouse-v0.2.0-macos-x86_64.tar.gz.sha256` — the conventional form,
///   the archive's own name with `.sha256` appended.
/// - `treehouse-v0.2.0-macos-x86_64.sha256` — the container extension stripped
///   *first*. This is what `.github/workflows/release.yml` writes
///   (`printf … > "treehouse-${TAG}-${{ matrix.suffix }}.sha256"`), and it is
///   what every release published so far carries.
///
/// Accepting only the first spelling was the defect that made `treehouse
/// update` fail on every platform: the workflow never published a file the
/// updater was willing to look at, so `check_latest_result` resolved a
/// `checksum_url` of `None` and `apply` refused to run. The updater is the
/// tolerant side on purpose — matching the stripped spelling costs nothing and
/// means no existing release has to be re-uploaded.
///
/// `every_published_asset_has_a_sidecar_the_updater_accepts` (in the
/// `release_workflow_contract` test module) ties this list back to the workflow
/// so the two cannot drift apart again.
fn checksum_sidecar_names(archive_name: &str) -> Vec<String> {
    let mut names = vec![format!("{archive_name}{CHECKSUM_SUFFIX}")];
    if let Some(stem) = strip_archive_ext(archive_name) {
        let stripped = format!("{stem}{CHECKSUM_SUFFIX}");
        if !names.contains(&stripped) {
            names.push(stripped);
        }
    }
    names
}

/// How good a match `candidate` is as the checksum file for `archive_name`:
/// `None` when it is not one at all, otherwise a lower-is-better rank.
///
/// A rank rather than a bare bool because a release carrying both spellings has
/// to resolve the same way on every run. The per-asset sidecars rank in
/// [`checksum_sidecar_names`] order — most specific first — and Go's aggregate
/// manifest ranks last, behind everything, because it is the only candidate
/// that covers more than this one archive.
fn checksum_rank(archive_name: &str, candidate: &str) -> Option<usize> {
    let names = checksum_sidecar_names(archive_name);
    names
        .iter()
        .position(|name| name == candidate)
        .or_else(|| (candidate == AGGREGATE_CHECKSUM_FILE).then_some(names.len()))
}

/// Whether `candidate` is a checksum file covering `archive_name`: one of this
/// release's per-asset sidecars ([`checksum_sidecar_names`]), or Go's aggregate
/// manifest if a release ships one instead.
fn is_checksum_for(archive_name: &str, candidate: &str) -> bool {
    checksum_rank(archive_name, candidate).is_some()
}

// ─── Version check with assets ────────────────────────────────────────────────

/// Checks for the latest release and resolves this platform's asset URLs.
///
/// Unlike [`check_latest`] this surfaces failures instead of swallowing them:
/// `treehouse update` must be able to tell the user the network is down rather
/// than exit 0. The update-check cache is *not* written here — the caller
/// decides when (Go writes it inside `CheckLatest`, updater.go:89-94).
pub fn check_latest_result(
    github_api_url: &str,
    current_version: &str,
    enforce_https: bool,
) -> Result<CheckResult, UpdateError> {
    let body = curl_capture(github_api_url, enforce_https, MAX_API_RESPONSE_SIZE)?;
    let release: GithubRelease = serde_json::from_str(&body)
        .map_err(|e| UpdateError::Fetch(format!("decoding release: {e}")))?;

    let archive = release
        .assets
        .iter()
        .find(|a| matches_current_platform_asset(&a.name));

    let checksum_url = archive.and_then(|archive| {
        // Every candidate is vetted by `is_checksum_for` and then ordered by
        // `checksum_rank`, so a release that somehow carries both sidecar
        // spellings resolves the same way on every run rather than in whatever
        // order GitHub happened to return the assets in.
        release
            .assets
            .iter()
            .filter(|c| is_checksum_for(&archive.name, &c.name))
            .min_by_key(|c| checksum_rank(&archive.name, &c.name))
            .map(|c| c.browser_download_url.clone())
    });

    let update_available = match (
        Version::parse(&release.tag_name),
        Version::parse(current_version),
    ) {
        (Some(latest), Some(cur)) => latest > cur,
        // Unparseable on either side: refuse to claim an update is available.
        // Guessing "yes" would send the CLI into a download it cannot verify.
        _ => false,
    };

    Ok(CheckResult {
        current_version: current_version.to_string(),
        latest_version: release.tag_name,
        update_available,
        asset_name: archive.map(|a| a.name.clone()),
        download_url: archive.map(|a| a.browser_download_url.clone()),
        checksum_url,
    })
}

/// [`check_latest_result`] plus the cache write, matching Go's `CheckLatest`
/// (which caches inside the check). For callers that want Go's exact shape.
pub fn check_and_cache(
    github_api_url: &str,
    current_version: &str,
    enforce_https: bool,
) -> Result<CheckResult, UpdateError> {
    let result = check_latest_result(github_api_url, current_version, enforce_https)?;
    write_cache(&result.latest_version);
    Ok(result)
}

// ─── Apply ────────────────────────────────────────────────────────────────────

/// Downloads, verifies, extracts and installs the release described by
/// `result`, replacing the running executable.
///
/// `enforce_https` is threaded in rather than read from a global so tests can
/// drive the whole pipeline off `file://` fixtures; production passes `true`.
///
/// # CLI usage
///
/// ```text
/// let result = updater::check_latest_result(DEFAULT_GITHUB_API_URL, VERSION, true)?;
/// if !result.update_available { /* "up to date" */ }
/// let applied = updater::apply(&result, true)?;   // prints "X -> Y" from `applied`
/// ```
///
/// Resolves `std::env::current_exe()`. Prefer [`apply_into`] when the target
/// is not the running binary (tests; packagers staging an install prefix).
pub fn apply(result: &CheckResult, enforce_https: bool) -> Result<Applied, UpdateError> {
    let exe = std::env::current_exe().map_err(|e| UpdateError::ResolveExecutable(e.to_string()))?;
    // EvalSymlinks equivalent. Skipping it would replace the resolved binary
    // while the user (or their package manager) is looking at a symlink, so
    // the next `treehouse` invocation follows the link back to the old file.
    let target = std::fs::canonicalize(&exe)
        .map_err(|e| UpdateError::ResolveExecutable(format!("resolving symlinks: {e}")))?;
    apply_into(result, enforce_https, &target)
}

/// [`apply`] against an explicit target path.
///
/// Split out so the full pipeline can be tested without touching the test
/// binary itself — replacing `current_exe()` mid-suite would corrupt every
/// other test in the process.
pub fn apply_into(
    result: &CheckResult,
    enforce_https: bool,
    target: &Path,
) -> Result<Applied, UpdateError> {
    let Some(url) = result.download_url.as_deref().filter(|u| !u.is_empty()) else {
        return Err(UpdateError::NoDownloadUrl {
            os: std::env::consts::OS,
            arch: std::env::consts::ARCH,
        });
    };
    require_https(url, enforce_https)
        .map_err(|e| UpdateError::InsecureUrl(format!("download URL: {e}")))?;

    // An unverified binary is not an update, it is arbitrary code execution
    // with a version string attached. Refuse before downloading anything.
    let Some(checksum_url) = result.checksum_url.as_deref().filter(|u| !u.is_empty()) else {
        return Err(UpdateError::Checksum(
            "no checksums file in release assets".to_string(),
        ));
    };
    require_https(checksum_url, enforce_https)
        .map_err(|e| UpdateError::InsecureUrl(format!("checksum URL: {e}")))?;

    let archive = download_to_temp(url, MAX_DOWNLOAD_SIZE)?;
    let sidecar = curl_capture(checksum_url, enforce_https, MAX_CHECKSUM_SIZE)
        .map_err(|e| UpdateError::Checksum(format!("downloading checksums: {e}")))?;
    verify_checksum(
        archive.path(),
        &sidecar,
        result.asset_name.as_deref().unwrap_or_default(),
    )?;

    let new_binary = extract_binary(archive.path())?;
    remove_quarantine(new_binary.path());

    atomic_replace(target, new_binary.path())?;

    Ok(Applied {
        from_version: result.current_version.clone(),
        to_version: result.latest_version.clone(),
        replaced_path: target.to_path_buf(),
    })
}

// ─── HTTP ─────────────────────────────────────────────────────────────────────

/// Rejects a non-HTTPS URL unless enforcement is off. Go `requireHTTPS`
/// (updater.go:442-453).
fn require_https(url: &str, enforce_https: bool) -> Result<(), UpdateError> {
    if !enforce_https {
        return Ok(());
    }
    if url.is_empty() {
        return Err(UpdateError::InsecureUrl("URL is empty".to_string()));
    }
    if !url.starts_with("https://") {
        return Err(UpdateError::InsecureUrl(format!(
            "refusing non-HTTPS URL: {url}"
        )));
    }
    Ok(())
}

/// Fetches a small text resource (release metadata, checksum sidecar) with curl.
///
/// curl rather than an HTTP client crate because this crate already shells out
/// to curl for the version check and the install path promises curl; a second
/// HTTP stack would be a second dependency tree to keep current for the same
/// job. `--max-filesize` bounds the declared-length case and the post-read
/// length check bounds the rest.
fn curl_capture(url: &str, enforce_https: bool, max_bytes: u64) -> Result<String, UpdateError> {
    require_https(url, enforce_https)?;
    let out = std::process::Command::new("curl")
        .args(["-fsSL", "--max-time", FETCH_TIMEOUT_SECS])
        .args(["--max-filesize", &max_bytes.to_string()])
        .arg(url)
        .output()
        .map_err(|e| UpdateError::Fetch(format!("invoking curl: {e}")))?;
    if !out.status.success() {
        return Err(UpdateError::Fetch(format!(
            "{url}: curl {} {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    if out.stdout.len() as u64 > max_bytes {
        return Err(UpdateError::Fetch(format!(
            "{url}: response exceeds maximum size of {max_bytes} bytes"
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Rejects a transfer that landed larger than its ceiling.
///
/// This is the backstop behind curl's `--max-filesize`, which only fires when
/// the response declares a Content-Length. A chunked response declares nothing,
/// so the bytes are counted here instead (Go does the same with
/// `io.LimitReader` + a post-copy length check, updater.go:560-568).
fn check_download_size(len: u64, max_bytes: u64) -> Result<(), UpdateError> {
    if len > max_bytes {
        return Err(UpdateError::Download(format!(
            "download exceeds maximum size of {max_bytes} bytes"
        )));
    }
    Ok(())
}

/// Streams `url` into a fresh temp file, refusing anything over `max_bytes`
/// (Go `downloadToTemp`, updater.go:542-571).
///
/// stdout goes straight to the file handle rather than through `.output()`:
/// `.output()` would buffer the whole asset in memory, and the cap exists
/// precisely because the size is not trusted.
fn download_to_temp(url: &str, max_bytes: u64) -> Result<tempfile::NamedTempFile, UpdateError> {
    let tmp = tempfile::Builder::new()
        .prefix("treehouse-update-")
        .tempfile()
        .map_err(|e| UpdateError::Download(format!("creating temp file: {e}")))?;

    let out = std::process::Command::new("curl")
        .args(["-fsSL", "--max-time", DOWNLOAD_TIMEOUT_SECS])
        // Aborts early on a declared oversize body.
        .args(["--max-filesize", &max_bytes.to_string()])
        .arg("-o")
        .arg(tmp.path())
        .arg(url)
        .stderr(std::process::Stdio::piped())
        .output()
        .map_err(|e| UpdateError::Download(format!("invoking curl: {e}")))?;
    if !out.status.success() {
        return Err(UpdateError::Download(format!(
            "{url}: curl {} {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }

    let len = std::fs::metadata(tmp.path())
        .map_err(|e| UpdateError::Download(format!("measuring download: {e}")))?
        .len();
    check_download_size(len, max_bytes)?;
    Ok(tmp)
}

// ─── Checksum ─────────────────────────────────────────────────────────────────

/// Extracts the expected sha256 for `asset_name` out of a checksum sidecar.
///
/// Sidecar lines are `sha256  filename`, exactly as `sha256sum -c` and the
/// release workflow write them. An exact filename match wins; failing that we
/// fall back to the platform match Go uses (updater.go:502-506) so an aggregate
/// `checksums.txt` still resolves. Anything that is not 64 hex digits is
/// treated as absent — a malformed line must fail the update, never be silently
/// compared as a string that happens to be equal.
fn checksum_for(sidecar: &str, asset_name: &str) -> Option<String> {
    let mut by_platform = None;
    for line in sidecar.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() != 2 || !is_sha256(fields[0]) {
            continue;
        }
        // `./name` is a legal sha256sum output when the file was hashed by path.
        let file = fields[1].strip_prefix("./").unwrap_or(fields[1]);
        if file == asset_name {
            return Some(fields[0].to_string());
        }
        if by_platform.is_none() && matches_current_platform_asset(file) {
            by_platform = Some(fields[0].to_string());
        }
    }
    by_platform
}

fn is_sha256(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Hashes a file with sha256 (Go `verifyChecksum` body, updater.go:512-526).
fn sha256_file(path: &Path) -> std::io::Result<String> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut hasher = <sha2::Sha256 as sha2::Digest>::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        sha2::Digest::update(&mut hasher, &buf[..n]);
    }
    Ok(to_hex(&sha2::Digest::finalize(hasher)))
}

fn to_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0f) as usize] as char);
    }
    s
}

/// Verifies `archive_path` against the sidecar's hash for `asset_name`.
fn verify_checksum(
    archive_path: &Path,
    sidecar: &str,
    asset_name: &str,
) -> Result<(), UpdateError> {
    let expected = checksum_for(sidecar, asset_name).ok_or_else(|| {
        UpdateError::Checksum(format!(
            "no checksum found for current platform asset{}",
            if asset_name.is_empty() {
                String::new()
            } else {
                format!(" ({asset_name})")
            }
        ))
    })?;
    let actual = sha256_file(archive_path)
        .map_err(|e| UpdateError::Checksum(format!("hashing download: {e}")))?;
    if actual != expected {
        return Err(UpdateError::Checksum(format!(
            "expected {expected}, got {actual}"
        )));
    }
    Ok(())
}

// ─── Extraction ───────────────────────────────────────────────────────────────

/// Pulls the executable out of the downloaded archive into a fresh temp file,
/// already `chmod 0755` (Go `extractBinary`).
///
/// The archive's own entry paths are never honoured — the entry is matched by
/// base name and its bytes are written to a temp file we chose. That makes
/// `../../.ssh/authorized_keys` style archive entries inert here rather than
/// something the caller has to defend against.
fn extract_binary(archive_path: &Path) -> Result<tempfile::NamedTempFile, UpdateError> {
    if cfg!(windows) {
        extract_zip(archive_path, binary_name())
    } else {
        extract_tar_gz(archive_path, binary_name())
    }
}

fn new_binary_temp() -> Result<tempfile::NamedTempFile, UpdateError> {
    tempfile::Builder::new()
        .prefix("treehouse-new-")
        .tempfile()
        .map_err(|e| UpdateError::Extract(format!("creating temp file: {e}")))
}

/// Copies one archive entry into a 0755 temp file, bounded by `max_bytes`.
///
/// The bound is a parameter so a test can prove it fires without building a
/// 100 MB fixture; production callers pass [`MAX_BINARY_SIZE`]. Streaming
/// matters here: the reader is a decompression stream, so an unbounded copy
/// would expand a small archive into as much disk as the attacker likes.
fn write_entry<R: std::io::Read>(
    reader: &mut R,
    temp: &mut tempfile::NamedTempFile,
    max_bytes: u64,
) -> Result<(), UpdateError> {
    use std::io::Read;
    let mut limited = reader.take(max_bytes + 1);
    let n = std::io::copy(&mut limited, temp.as_file_mut())
        .map_err(|e| UpdateError::Extract(format!("reading archive: {e}")))?;
    if n > max_bytes {
        return Err(UpdateError::Extract(format!(
            "binary exceeds maximum size of {max_bytes} bytes"
        )));
    }
    temp.as_file_mut()
        .sync_all()
        .map_err(|e| UpdateError::Extract(format!("syncing temp file: {e}")))?;
    set_executable(temp.path())
}

#[cfg(unix)]
fn set_executable(path: &Path) -> Result<(), UpdateError> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .map_err(|e| UpdateError::Extract(format!("setting mode 0755: {e}")))
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) -> Result<(), UpdateError> {
    // Windows has no execute bit; the extension decides what runs.
    Ok(())
}

/// `want` is the entry's base name to look for — see [`binary_name`]. It is a
/// parameter rather than a constant so the Windows `.exe` branch is exercised
/// by the same tests on Linux and macOS, which never publish a zip.
fn extract_tar_gz(archive_path: &Path, want: &str) -> Result<tempfile::NamedTempFile, UpdateError> {
    let file = std::fs::File::open(archive_path)
        .map_err(|e| UpdateError::Extract(format!("opening archive: {e}")))?;
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(file));

    let entries = archive
        .entries()
        .map_err(|e| UpdateError::Extract(format!("reading archive: {e}")))?;
    for entry in entries {
        let mut entry = entry.map_err(|e| UpdateError::Extract(format!("reading archive: {e}")))?;
        // Go requires tar.TypeReg (updater.go:606): a *directory* named
        // `treehouse` must not be mistaken for the binary.
        if !entry.header().entry_type().is_file() {
            continue;
        }
        // Read the entry's own path but never act on it — the match is on the
        // base name and the bytes go to a temp file this function chose, so a
        // `../../…/treehouse` entry is inert here rather than a write primitive.
        let name = entry
            .path()
            .map_err(|e| UpdateError::Extract(format!("bad entry path: {e}")))?;
        if name.file_name() != Some(std::ffi::OsStr::new(want)) {
            continue;
        }
        let mut temp = new_binary_temp()?;
        write_entry(&mut entry, &mut temp, MAX_BINARY_SIZE)?;
        return Ok(temp);
    }
    Err(UpdateError::Extract(format!(
        "binary {want:?} not found in archive"
    )))
}

/// `want` is the entry's base name to look for — see [`binary_name`].
fn extract_zip(archive_path: &Path, want: &str) -> Result<tempfile::NamedTempFile, UpdateError> {
    let file = std::fs::File::open(archive_path)
        .map_err(|e| UpdateError::Extract(format!("opening archive: {e}")))?;
    let mut archive = zip::ZipArchive::new(file)
        .map_err(|e| UpdateError::Extract(format!("opening archive: {e}")))?;

    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| UpdateError::Extract(format!("reading archive: {e}")))?;
        if !entry.is_file() {
            continue;
        }
        // As in the tar path: the archive's path is inspected, never honoured.
        let name = std::path::Path::new(entry.name());
        if name.file_name() != Some(std::ffi::OsStr::new(want)) {
            continue;
        }
        let mut temp = new_binary_temp()?;
        write_entry(&mut entry, &mut temp, MAX_BINARY_SIZE)?;
        return Ok(temp);
    }
    Err(UpdateError::Extract(format!(
        "binary {want:?} not found in archive"
    )))
}

/// Drops the macOS quarantine xattr from the freshly extracted binary.
///
/// A quarantined binary is killed on first exec, which turns a successful
/// update into "the binary I just installed will not run". Best-effort, as in
/// Go (quarantine_darwin.go:8-12): an asset that was never quarantined has no
/// xattr, and failing the update over that would be wrong.
#[cfg(target_os = "macos")]
fn remove_quarantine(path: &Path) {
    let _ = std::process::Command::new("xattr")
        .args(["-d", "com.apple.quarantine"])
        .arg(path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

#[cfg(not(target_os = "macos"))]
fn remove_quarantine(_path: &Path) {}

// ─── Replacement ──────────────────────────────────────────────────────────────

/// Installs `new_binary` at `target`, atomically where the platform allows.
///
/// Mirrors Go `atomicReplace` (updater.go:679-733) including its two
/// escape hatches, because both are load-bearing:
///
/// 1. The temp file is created **in the target's own directory**. A temp file
///    in `/tmp` would make the final rename a cross-device copy — not atomic,
///    and a crash mid-copy leaves a half-written executable at the target path.
/// 2. If the directory is not writable (`/usr/local/bin` owned by root) but the
///    file is, we fall back to overwriting in place. Not atomic, and Go accepts
///    that trade: a failed update is worse than a brief window.
fn atomic_replace(target: &Path, new_binary: &Path) -> Result<(), UpdateError> {
    let info = std::fs::metadata(target)
        .map_err(|e| UpdateError::Replace(format!("stating target {}: {e}", target.display())))?;

    let dir = target.parent().unwrap_or_else(|| Path::new("."));
    let tmp = match tempfile::Builder::new()
        .prefix(".treehouse-update-")
        .tempfile_in(dir)
    {
        Ok(t) => t,
        // No write access to the directory. Go falls back to direct overwrite
        // here too; anything else would make `treehouse update` fail for every
        // user who installed into a root-owned prefix.
        Err(_) => return direct_overwrite(target, new_binary, mode_of(info.permissions())),
    };

    let tmp_path = tmp.into_temp_path();
    let mut src = std::fs::File::open(new_binary)
        .map_err(|e| UpdateError::Replace(format!("opening new binary: {e}")))?;
    let mut dst = std::fs::File::create(&tmp_path)
        .map_err(|e| UpdateError::Replace(format!("opening staged binary: {e}")))?;
    if let Err(e) = std::io::copy(&mut src, &mut dst) {
        return Err(UpdateError::Replace(format!("staging new binary: {e}")));
    }
    // Data before metadata before rename. `rename` is atomic with respect to
    // other processes, but it makes no promise about the bytes it points at;
    // without this sync a crash can leave a zero-length "new" binary in place.
    if let Err(e) = dst.sync_all() {
        return Err(UpdateError::Replace(format!("syncing staged binary: {e}")));
    }
    drop(dst);
    // Carry the *original* binary's mode over: the extracted file is 0755, but
    // a user's installed binary may be 0750 or group-writable by design.
    std::fs::set_permissions(&tmp_path, info.permissions())
        .map_err(|e| UpdateError::Replace(format!("setting mode: {e}")))?;

    if let Err(e) = std::fs::rename(&tmp_path, target) {
        // Windows maps a running .exe and refuses to overwrite it in place,
        // but a mapped image CAN be renamed aside. Go took the same exit.
        #[cfg(windows)]
        if replace_running_windows(target, &tmp_path).is_ok() {
            return Ok(());
        }
        let _ = e;
        return Err(UpdateError::Replace(format!(
            "renaming onto {}: {e}",
            target.display()
        )));
    }
    Ok(())
}

/// Windows-only escape hatch (Go `replaceRunningWindows`, updater.go:742-753):
/// move the locked image aside, then place the new binary at its original path.
///
/// Not `#[cfg]`-gated so the naming rule below — the part Go actually got wrong
/// once — is covered by the test suite on every platform.
///
/// The backup name is unique **per attempt** (`<target>.old.<pid>.<nanos>`). A
/// fixed `.old` name cannot be reused: a previous update's `.old` may still be
/// mapped by a still-running process, and renaming onto it fails. That was Go
/// issue #121.
//
// Compiled on every platform, not just Windows, so the naming rule is type-checked
// by the Linux/macOS build and covered by tests there; only the call site in
// [`atomic_replace`] is Windows-gated.
#[cfg_attr(not(windows), allow(dead_code))]
fn replace_running_windows(target: &Path, staged: &Path) -> std::io::Result<()> {
    let old_path = windows_backup_path(target);
    std::fs::rename(target, &old_path)?;
    if let Err(e) = std::fs::rename(staged, target) {
        // Put the old image back: leaving it aside would delete the user's
        // working binary as a side effect of a failed update.
        let _ = std::fs::rename(&old_path, target);
        return Err(e);
    }
    let _ = std::fs::remove_file(&old_path);
    Ok(())
}

#[cfg_attr(not(windows), allow(dead_code))]
fn windows_backup_path(target: &Path) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let name = target.file_name().unwrap_or_default().to_string_lossy();
    target.with_file_name(format!("{name}.old.{}.{nanos}", std::process::id()))
}

/// Last-resort, non-atomic install for a target directory we cannot write to.
///
/// There is a window where `target` is a truncated binary. It exists because the
/// alternative is refusing to update at all for anyone who installed into a
/// root-owned `/usr/local/bin` while owning the file (Go `directOverwrite`,
/// updater.go:760-778).
fn direct_overwrite(
    target: &Path,
    new_binary: &Path,
    mode: std::fs::Permissions,
) -> Result<(), UpdateError> {
    let mut src = std::fs::File::open(new_binary)
        .map_err(|e| UpdateError::Replace(format!("opening new binary: {e}")))?;
    let mut dst = std::fs::File::create(target)
        .map_err(|e| UpdateError::Replace(format!("opening target: {e}")))?;
    if let Err(e) = std::io::copy(&mut src, &mut dst) {
        return Err(UpdateError::Replace(format!("writing target: {e}")));
    }
    std::fs::set_permissions(target, mode)
        .map_err(|e| UpdateError::Replace(format!("setting mode: {e}")))
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

    // ─── _with_env tests ────────────────────────────────────────────────────

    #[test]
    fn cache_path_with_env_returns_env_path() {
        let env = crate::env::InMemoryEnv::new(std::path::PathBuf::from("/test"));
        assert_eq!(
            cache_path_with_env(&env),
            Some(std::path::PathBuf::from("/test/cache/update-check.json"))
        );
    }

    #[test]
    fn write_read_cache_roundtrip_with_env() {
        let env = crate::env::InMemoryEnv::new(std::path::PathBuf::from("/test"));
        write_cache_with_env(&env, "2.0.0");
        let cache = read_cache_with_env(&env).unwrap();
        assert_eq!(cache.latest_version, "2.0.0");
    }

    #[test]
    fn read_cache_with_env_missing_returns_none() {
        let env = crate::env::InMemoryEnv::new(std::path::PathBuf::from("/test"));
        assert!(read_cache_with_env(&env).is_none());
    }

    #[test]
    fn is_cache_stale_with_env_missing_returns_true() {
        let env = crate::env::InMemoryEnv::new(std::path::PathBuf::from("/test"));
        assert!(is_cache_stale_with_env("1.0.0", &env));
    }

    #[test]
    fn update_available_with_env_checks_cache() {
        let env = crate::env::InMemoryEnv::new(std::path::PathBuf::from("/test"));
        assert!(!update_available_with_env("1.0.0", &env)); // no cache
        write_cache_with_env(&env, "2.0.0");
        assert!(update_available_with_env("1.0.0", &env)); // 2.0.0 > 1.0.0
        assert!(!update_available_with_env("3.0.0", &env)); // 2.0.0 < 3.0.0
    }

    // ─── Apply pipeline ────────────────────────────────────────────────────
    //
    // Fixtures are built on disk and served over `file://` so the whole
    // pipeline — download, verify, extract, replace — runs with no network.

    /// The five assets `.github/workflows/release.yml` actually publishes,
    /// each paired with the (os, arch) that must select it.
    const RELEASE_MATRIX: &[(&str, &str, &str)] = &[
        ("linux-x86_64", "linux", "x86_64"),
        ("linux-aarch64", "linux", "aarch64"),
        ("macos-x86_64", "macos", "x86_64"),
        ("macos-aarch64", "macos", "aarch64"),
        ("windows-x86_64", "windows", "x86_64"),
    ];

    #[test]
    fn asset_matcher_covers_every_release_matrix_entry() {
        for (suffix, os, arch) in RELEASE_MATRIX {
            let ext = if *os == "windows" { "zip" } else { "tar.gz" };
            let name = format!("treehouse-v0.1.2-{suffix}.{ext}");
            assert!(
                matches_asset(&name, os, arch, ext),
                "{name} should match {os}/{arch}"
            );
            // A different platform's asset must not match, or `update` would
            // install a binary built for the wrong architecture.
            for (_, other_os, other_arch) in RELEASE_MATRIX {
                if *other_os != *os || *other_arch != *arch {
                    assert!(
                        !matches_asset(&name, other_os, other_arch, ext),
                        "{name} must not match {other_os}/{other_arch}"
                    );
                }
            }
        }
    }

    #[test]
    fn asset_matcher_rejects_wrong_extension_and_prefix() {
        // The zip and tar.gz of one platform differ only in extension; picking
        // the wrong one means extracting the wrong container.
        assert!(!matches_asset(
            "treehouse-v0.1.2-linux-x86_64.zip",
            "linux",
            "x86_64",
            "tar.gz"
        ));
        // Checksum sidecars are not the archive.
        assert!(!matches_asset(
            "treehouse-v0.1.2-linux-x86_64.tar.gz.sha256",
            "linux",
            "x86_64",
            "tar.gz"
        ));
        assert!(!matches_asset(
            "something-else.tar.gz",
            "linux",
            "x86_64",
            "tar.gz"
        ));
    }

    #[test]
    fn checksum_for_reads_sidecar_exact_match() {
        // The asset this platform would actually be published under comes
        // first; the exact-match rule must still pick the second line. A
        // matcher that returned "the first line that looks like ours" would
        // happily install a hash belonging to a different file.
        let ours = format!("treehouse-v9.9.9-{}.tar.gz", os_arch_suffix());
        let other_version = format!("treehouse-v8.8.8-{}.tar.gz", os_arch_suffix());
        let exact_hash = "a".repeat(64);
        let sidecar = format!(
            "{}  {other}\n{exact_hash}  {ours}\n",
            "b".repeat(64),
            exact_hash = exact_hash,
            ours = ours,
            other = other_version,
        );
        assert_eq!(checksum_for(&sidecar, &ours), Some(exact_hash));
    }

    #[test]
    fn checksum_for_tolerates_leading_dot_slash() {
        let hash = "c".repeat(64);
        let sidecar = format!("{hash}  ./treehouse-v1.0.0-linux-x86_64.tar.gz\n");
        assert_eq!(
            checksum_for(&sidecar, "treehouse-v1.0.0-linux-x86_64.tar.gz"),
            Some(hash)
        );
    }

    #[test]
    fn checksum_for_rejects_malformed_hash() {
        // A truncated or non-hex line must read as "absent" (fail closed),
        // never as a hash that could accidentally compare equal.
        let sidecar = "deadbeef  treehouse-v1.0.0-linux-x86_64.tar.gz\n";
        assert_eq!(
            checksum_for(sidecar, "treehouse-v1.0.0-linux-x86_64.tar.gz"),
            None
        );
        let upper = format!("{}  treehouse-v1.0.0-linux-x86_64.tar.gz\n", "A".repeat(64));
        assert_eq!(
            checksum_for(&upper, "treehouse-v1.0.0-linux-x86_64.tar.gz"),
            None
        );
    }

    #[test]
    fn checksum_for_missing_asset_is_none() {
        let sidecar = format!("{}  other-file.tar.gz\n", "a".repeat(64));
        assert_eq!(
            checksum_for(&sidecar, "treehouse-v1.0.0-linux-x86_64.tar.gz"),
            None
        );
    }

    #[test]
    fn verify_checksum_accepts_match_and_rejects_tamper() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("treehouse.tar.gz");
        std::fs::write(&archive, b"hello archive").unwrap();
        let hash = sha256_file(&archive).unwrap();
        let name = "treehouse-v1.0.0-linux-x86_64.tar.gz";
        let sidecar = format!("{hash}  {name}\n");

        verify_checksum(&archive, &sidecar, name).unwrap();

        // One flipped byte in the download must fail the whole update.
        std::fs::write(&archive, b"hello archivf").unwrap();
        let err = verify_checksum(&archive, &sidecar, name).unwrap_err();
        assert!(matches!(err, UpdateError::Checksum(_)), "got {err:?}");
    }

    /// Builds a tar.gz containing `contents` under `entry_name`.
    fn make_tar_gz(entry_name: &str, contents: &[u8]) -> Vec<u8> {
        let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
            Vec::new(),
            flate2::Compression::fast(),
        ));
        let mut header = tar::Header::new_gnu();
        header.set_size(contents.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        builder
            .append_data(&mut header, entry_name, contents)
            .unwrap();
        builder.into_inner().unwrap().finish().unwrap()
    }

    /// Builds a tar.gz whose single entry sits at a traversal path.
    ///
    /// `tar::Builder` refuses to author one (correctly), so the 512-byte header
    /// is written by hand — otherwise nothing could ever test the reader
    /// against a hostile archive.
    fn make_tar_gz_traversal(entry_name: &str, contents: &[u8]) -> Vec<u8> {
        let mut header = tar::Header::new_gnu();
        header.set_size(contents.len() as u64);
        header.set_mode(0o755);
        {
            let old = header.as_old_mut();
            old.name[..entry_name.len()].copy_from_slice(entry_name.as_bytes());
        }
        header.set_cksum();

        let mut raw = Vec::new();
        raw.extend_from_slice(header.as_bytes());
        raw.extend_from_slice(contents);
        raw.resize(512 + contents.len().next_multiple_of(512), 0);
        raw.extend_from_slice(&[0u8; 1024]); // end-of-archive marker

        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        std::io::Write::write_all(&mut enc, &raw).unwrap();
        enc.finish().unwrap()
    }

    /// Builds a tar.gz containing a single *directory* entry named `entry_name`.
    fn make_tar_gz_dir(entry_name: &str) -> Vec<u8> {
        let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
            Vec::new(),
            flate2::Compression::fast(),
        ));
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Directory);
        header.set_size(0);
        header.set_mode(0o755);
        builder
            .append_data(&mut header, entry_name, std::io::empty())
            .unwrap();
        builder.into_inner().unwrap().finish().unwrap()
    }

    /// Builds a stored (uncompressed) zip containing `contents` under `entry_name`.
    fn make_zip(entry_name: &str, contents: &[u8]) -> Vec<u8> {
        use std::io::Write;
        let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        w.start_file(
            entry_name,
            zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored)
                .unix_permissions(0o755),
        )
        .unwrap();
        w.write_all(contents).unwrap();
        w.finish().unwrap().into_inner()
    }

    #[test]
    fn extract_tar_gz_writes_binary_and_sets_mode_0755() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("a.tar.gz");
        // Nested like the real archive (`tar -czf ... -C dirname treehouse`).
        std::fs::write(&archive, make_tar_gz("treehouse", b"NEW BINARY")).unwrap();

        let out = extract_tar_gz(&archive, "treehouse").unwrap();
        assert_eq!(std::fs::read(out.path()).unwrap(), b"NEW BINARY");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(out.path()).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o755, "extracted binary must be executable");
        }
    }

    #[test]
    fn extract_tar_gz_finds_nested_entry() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("a.tar.gz");
        std::fs::write(&archive, make_tar_gz("dist/treehouse", b"NESTED BINARY")).unwrap();
        let out = extract_tar_gz(&archive, "treehouse").unwrap();
        assert_eq!(std::fs::read(out.path()).unwrap(), b"NESTED BINARY");
    }

    #[test]
    fn extract_tar_gz_ignores_traversal_paths() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("a.tar.gz");
        // A hostile archive naming the treehouse binary at a traversal path
        // must still be extracted to a temp file we chose, never to that path.
        std::fs::write(
            &archive,
            make_tar_gz_traversal("../../../../tmp/treehouse", b"ESCAPED"),
        )
        .unwrap();
        let out = extract_tar_gz(&archive, "treehouse").unwrap();
        assert_eq!(
            std::fs::canonicalize(out.path().parent().unwrap()).unwrap(),
            std::fs::canonicalize(std::env::temp_dir()).unwrap(),
            "extraction must land in our temp dir, not at the archive's path"
        );
        assert_eq!(std::fs::read(out.path()).unwrap(), b"ESCAPED");
    }

    #[test]
    fn extract_tar_gz_errors_when_binary_absent() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("a.tar.gz");
        std::fs::write(&archive, make_tar_gz("README.md", b"nope")).unwrap();
        let err = extract_tar_gz(&archive, "treehouse").unwrap_err();
        assert!(matches!(err, UpdateError::Extract(_)), "got {err:?}");
        assert!(
            err.to_string().contains("not found in archive"),
            "got {err}"
        );
    }

    #[test]
    fn extract_tar_gz_skips_a_directory_named_like_the_binary() {
        // Go requires tar.TypeReg (updater.go:606). A directory entry whose
        // base name matches must not be extracted as the executable.
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("a.tar.gz");
        let bytes = make_tar_gz_dir("treehouse/");
        std::fs::write(&archive, bytes).unwrap();
        let err = extract_tar_gz(&archive, "treehouse").unwrap_err();
        assert!(
            err.to_string().contains("not found in archive"),
            "got {err}"
        );
    }

    #[test]
    fn extract_zip_writes_binary() {
        // Runs on every platform, not just Windows: this is the branch the
        // Linux/macOS CI can never reach through `extract_binary`.
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("a.zip");
        std::fs::write(&archive, make_zip("treehouse.exe", b"WINDOWS BINARY")).unwrap();
        let out = extract_zip(&archive, "treehouse.exe").unwrap();
        assert_eq!(std::fs::read(out.path()).unwrap(), b"WINDOWS BINARY");
    }

    #[test]
    fn write_entry_refuses_an_oversized_binary() {
        // A small archive that expands without bound is the shape of a
        // decompression bomb; the cap on the extracted bytes is what stops it,
        // and it has to stop it while streaming, not after.
        let mut payload = vec![0u8; 64 * 1024];
        payload.extend_from_slice(b"tail past the limit");
        let mut temp = new_binary_temp().unwrap();
        let err = write_entry(&mut payload.as_slice(), &mut temp, 1024).unwrap_err();
        assert!(
            err.to_string().contains("exceeds maximum size"),
            "got {err}"
        );
    }

    #[test]
    fn write_entry_accepts_a_binary_exactly_at_the_limit() {
        let payload = vec![0u8; 4096];
        let mut temp = new_binary_temp().unwrap();
        write_entry(&mut payload.as_slice(), &mut temp, 4096).unwrap();
        assert_eq!(std::fs::metadata(temp.path()).unwrap().len(), 4096);
    }

    #[test]
    fn extract_zip_ignores_traversal_paths() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("a.zip");
        std::fs::write(
            &archive,
            make_zip("../../../../tmp/treehouse.exe", b"ESCAPED"),
        )
        .unwrap();
        let out = extract_zip(&archive, "treehouse.exe").unwrap();
        assert_eq!(
            std::fs::canonicalize(out.path().parent().unwrap()).unwrap(),
            std::fs::canonicalize(std::env::temp_dir()).unwrap()
        );
        assert_eq!(std::fs::read(out.path()).unwrap(), b"ESCAPED");
    }

    #[test]
    fn extract_zip_errors_when_binary_absent() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("a.zip");
        std::fs::write(&archive, make_zip("other.txt", b"nope")).unwrap();
        let err = extract_zip(&archive, "treehouse.exe").unwrap_err();
        assert!(
            err.to_string().contains("not found in archive"),
            "got {err}"
        );
    }

    #[test]
    fn extract_binary_dispatches_on_the_current_platform() {
        let dir = tempfile::tempdir().unwrap();
        if cfg!(windows) {
            let archive = dir.path().join("a.zip");
            std::fs::write(&archive, make_zip("treehouse.exe", b"WIN")).unwrap();
            assert_eq!(
                std::fs::read(extract_binary(&archive).unwrap().path()).unwrap(),
                b"WIN"
            );
        } else {
            let archive = dir.path().join("a.tar.gz");
            std::fs::write(&archive, make_tar_gz("treehouse", b"UNIX")).unwrap();
            assert_eq!(
                std::fs::read(extract_binary(&archive).unwrap().path()).unwrap(),
                b"UNIX"
            );
        }
    }

    #[test]
    fn atomic_replace_swaps_contents_and_keeps_original_mode() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("treehouse");
        let staged = dir.path().join("staged");
        std::fs::write(&target, b"OLD BINARY").unwrap();
        std::fs::write(&staged, b"NEW BINARY").unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o750)).unwrap();
        }

        atomic_replace(&target, &staged).unwrap();

        assert_eq!(std::fs::read(&target).unwrap(), b"NEW BINARY");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&target).unwrap().permissions().mode();
            assert_eq!(
                mode & 0o777,
                0o750,
                "install must carry the original binary's mode, not 0755"
            );
        }
        // The staging file lives in the target's directory so the final rename
        // is same-filesystem; it must not survive the update.
        let strays: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with(".treehouse-update-"))
            .collect();
        assert!(strays.is_empty(), "stray staging files: {strays:?}");
    }

    #[test]
    fn atomic_replace_errors_on_missing_target_and_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("does-not-exist");
        let staged = dir.path().join("staged");
        std::fs::write(&staged, b"NEW").unwrap();

        let err = atomic_replace(&target, &staged).unwrap_err();
        assert!(matches!(err, UpdateError::Replace(_)), "got {err:?}");
        assert!(
            !target.exists(),
            "a failed replace must not create the target"
        );
    }

    #[test]
    fn atomic_replace_falls_back_when_target_dir_is_not_writable() {
        // The `/usr/local/bin` case: directory owned by root, file owned by the
        // user. Failing here would make `update` impossible for them, so Go
        // falls back to a non-atomic overwrite and so must we.
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("treehouse");
        let staged = dir.path().join("staged");
        std::fs::write(&target, b"OLD").unwrap();
        std::fs::write(&staged, b"NEW").unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o555)).unwrap();
            let result = atomic_replace(&target, &staged);
            // Restore before asserting so the tempdir can clean itself up.
            std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
            result.unwrap();
            assert_eq!(std::fs::read(&target).unwrap(), b"NEW");
        }
        #[cfg(not(unix))]
        let _ = (target, staged);
    }

    #[test]
    fn windows_backup_name_is_unique_per_attempt() {
        // Go issue #121: a fixed `.old` path cannot be reused because a prior
        // update's backup may still be mapped by a running process.
        let target = Path::new("/usr/local/bin/treehouse");
        let a = windows_backup_path(target);
        let b = windows_backup_path(target);
        assert_ne!(a, b, "backup name must differ between attempts");
        for p in [&a, &b] {
            let name = p.file_name().unwrap().to_string_lossy();
            assert!(name.starts_with("treehouse.old."), "got {name}");
            assert_eq!(p.parent(), target.parent());
        }
    }

    #[test]
    fn replace_running_windows_swaps_content_and_cleans_up() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("treehouse.exe");
        let staged = dir.path().join("staged.exe");
        std::fs::write(&target, b"LOCKED").unwrap();
        std::fs::write(&staged, b"NEW").unwrap();

        replace_running_windows(&target, &staged).unwrap();

        assert_eq!(std::fs::read(&target).unwrap(), b"NEW");
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".old."))
            .collect();
        assert!(leftovers.is_empty(), "backup not cleaned: {leftovers:?}");
    }

    #[test]
    fn replace_running_windows_rolls_back_when_staging_lost() {
        // The second rename failing must not leave the user's working binary
        // sitting under a `.old.` name — that would delete it as a side effect.
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("treehouse.exe");
        std::fs::write(&target, b"LOCKED").unwrap();
        let missing = dir.path().join("never-created.exe");

        assert!(replace_running_windows(&target, &missing).is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"LOCKED");
    }

    // ─── apply / apply_into ─────────────────────────────────────────────────

    fn file_url(path: &Path) -> String {
        format!("file://{}", path.display())
    }

    /// Writes an archive + sidecar pair into `dir` and returns the CheckResult
    /// that points at them.
    fn fixture(dir: &Path, contents: &[u8]) -> CheckResult {
        // The container and the entry name have to be the ones this platform
        // actually publishes. `extract_binary` opens a zip and looks for
        // `treehouse.exe` on Windows (Go `extractBinary`, updater.go:573-578),
        // so a hard-coded tar.gz is read as a zip there and dies with
        // "Could not find EOCD" before the update can be asserted on.
        let archive_name = format!("treehouse-v9.9.9-{}.{}", os_arch_suffix(), archive_ext());
        let archive = dir.join(&archive_name);
        let archive_bytes = if cfg!(windows) {
            make_zip(binary_name(), contents)
        } else {
            make_tar_gz(binary_name(), contents)
        };
        std::fs::write(&archive, &archive_bytes).unwrap();

        let sidecar_name = format!("{archive_name}.sha256");
        let sidecar = dir.join(&sidecar_name);
        let hash = sha256_file(&archive).unwrap();
        std::fs::write(&sidecar, format!("{hash}  {archive_name}\n")).unwrap();

        CheckResult {
            current_version: "0.1.1".to_string(),
            latest_version: "v9.9.9".to_string(),
            update_available: true,
            asset_name: Some(archive_name),
            download_url: Some(file_url(&archive)),
            checksum_url: Some(file_url(&sidecar)),
        }
    }

    /// The archive name suffix this platform would actually be published under.
    fn os_arch_suffix() -> String {
        format!(
            "{}-{}",
            match std::env::consts::OS {
                "macos" => "macos",
                other => other,
            },
            std::env::consts::ARCH
        )
    }

    #[test]
    fn apply_into_replaces_the_target_end_to_end() {
        // The M-027 regression: an update must actually change the bytes on
        // disk, and must report the path it changed.
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("treehouse");
        std::fs::write(&target, b"OLD BINARY").unwrap();

        let result = fixture(dir.path(), b"NEW BINARY");
        let applied = apply_into(&result, false, &target).unwrap();

        assert_eq!(std::fs::read(&target).unwrap(), b"NEW BINARY");
        assert_eq!(applied.from_version, "0.1.1");
        assert_eq!(applied.to_version, "v9.9.9");
        assert_eq!(applied.replaced_path, target);
    }

    #[test]
    fn apply_into_is_idempotent_on_a_second_run() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("treehouse");
        std::fs::write(&target, b"OLD").unwrap();
        let result = fixture(dir.path(), b"NEW");

        apply_into(&result, false, &target).unwrap();
        // A second update over an already-updated binary must still land.
        apply_into(&result, false, &target).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"NEW");
    }

    #[test]
    fn apply_into_leaves_target_untouched_on_checksum_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("treehouse");
        std::fs::write(&target, b"OLD BINARY").unwrap();

        let mut result = fixture(dir.path(), b"NEW BINARY");
        // Corrupt the sidecar after hashing: the archive no longer matches.
        result.checksum_url = Some(file_url(&dir.path().join("does-not-exist.sha256")));
        let err = apply_into(&result, false, &target).unwrap_err();
        assert!(matches!(err, UpdateError::Checksum(_)), "got {err:?}");
        assert_eq!(
            std::fs::read(&target).unwrap(),
            b"OLD BINARY",
            "a failed verification must never touch the installed binary"
        );
    }

    #[test]
    fn apply_into_leaves_target_untouched_when_archive_has_no_binary() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("treehouse");
        std::fs::write(&target, b"OLD BINARY").unwrap();

        let name = format!("treehouse-v9.9.9-{}.{}", os_arch_suffix(), archive_ext());
        let archive = dir.path().join(&name);
        let bytes = if cfg!(windows) {
            make_zip("README.md", b"no binary here")
        } else {
            make_tar_gz("README.md", b"no binary here")
        };
        std::fs::write(&archive, bytes).unwrap();
        let hash = sha256_file(&archive).unwrap();
        let sidecar = dir.path().join(format!("{name}.sha256"));
        std::fs::write(&sidecar, format!("{hash}  {name}\n")).unwrap();

        let result = CheckResult {
            current_version: "0.1.1".to_string(),
            latest_version: "v9.9.9".to_string(),
            update_available: true,
            asset_name: Some(name),
            download_url: Some(file_url(&archive)),
            checksum_url: Some(file_url(&sidecar)),
        };
        let err = apply_into(&result, false, &target).unwrap_err();
        assert!(matches!(err, UpdateError::Extract(_)), "got {err:?}");
        assert_eq!(std::fs::read(&target).unwrap(), b"OLD BINARY");
    }

    #[test]
    fn apply_into_errors_when_no_asset_for_platform() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("treehouse");
        std::fs::write(&target, b"OLD").unwrap();
        let result = CheckResult::default();
        let err = apply_into(&result, true, &target).unwrap_err();
        assert!(
            matches!(err, UpdateError::NoDownloadUrl { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn apply_into_refuses_plain_http_download() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("treehouse");
        let result = CheckResult {
            download_url: Some("http://evil.example/treehouse.tar.gz".to_string()),
            checksum_url: Some("https://ok.example/treehouse.tar.gz.sha256".to_string()),
            ..Default::default()
        };
        let err = apply_into(&result, true, &target).unwrap_err();
        assert!(matches!(err, UpdateError::InsecureUrl(_)), "got {err:?}");
        assert!(err.to_string().contains("download URL"), "got {err}");
        assert!(!target.exists());
    }

    #[test]
    fn apply_into_refuses_plain_http_checksum() {
        // The asset is on https but the hash is not: an attacker who can
        // rewrite the sidecar picks the hash, so both must be pinned.
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("treehouse");
        let result = CheckResult {
            download_url: Some("https://ok.example/treehouse.tar.gz".to_string()),
            checksum_url: Some("http://evil.example/treehouse.tar.gz.sha256".to_string()),
            ..Default::default()
        };
        let err = apply_into(&result, true, &target).unwrap_err();
        assert!(matches!(err, UpdateError::InsecureUrl(_)), "got {err:?}");
        assert!(err.to_string().contains("checksum URL"), "got {err}");
    }

    #[test]
    fn apply_into_refuses_to_update_without_a_checksum_file() {
        // Go updater.go:483-485. Without a sidecar there is nothing to verify
        // against, so the update is refused outright.
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("treehouse");
        let result = CheckResult {
            download_url: Some("https://ok.example/treehouse.tar.gz".to_string()),
            ..Default::default()
        };
        let err = apply_into(&result, true, &target).unwrap_err();
        assert!(matches!(err, UpdateError::Checksum(_)), "got {err:?}");
    }

    #[test]
    fn download_to_temp_rejects_oversize() {
        let dir = tempfile::tempdir().unwrap();
        let big = dir.path().join("big.bin");
        std::fs::write(&big, vec![0u8; 4096]).unwrap();
        // curl aborts first here because the local file has a declared size;
        // either way the update must fail rather than extract 4 KB of "binary".
        let err = download_to_temp(&file_url(&big), 1024).unwrap_err();
        assert!(matches!(err, UpdateError::Download(_)), "got {err:?}");
    }

    #[test]
    fn download_size_cap_is_enforced_independently_of_curl() {
        // The backstop for a chunked response, where curl's --max-filesize has
        // no declared length to act on.
        check_download_size(1024, 1024).unwrap();
        let err = check_download_size(1025, 1024).unwrap_err();
        assert!(
            err.to_string().contains("exceeds maximum size"),
            "got {err}"
        );
    }

    #[test]
    fn download_to_temp_accepts_a_within_limit_file() {
        let dir = tempfile::tempdir().unwrap();
        let ok = dir.path().join("ok.bin");
        std::fs::write(&ok, vec![7u8; 512]).unwrap();
        let tmp = download_to_temp(&file_url(&ok), 4096).unwrap();
        assert_eq!(std::fs::read(tmp.path()).unwrap(), vec![7u8; 512]);
    }

    #[test]
    fn check_latest_result_rejects_plain_http() {
        let err = check_latest_result("http://insecure.example", "1.0.0", true).unwrap_err();
        assert!(matches!(err, UpdateError::InsecureUrl(_)), "got {err:?}");
    }
}

// ─── release-workflow contract ────────────────────────────────────────────────
//
// `.github/workflows/release.yml` decides the names of the assets we download;
// this module decides which of those names we will accept. Nothing in the type
// system joins them, so the only thing that has ever stopped them drifting
// apart is a test that reads the real workflow. This is that test.
//
// It reads the YAML as TEXT and parses the `tar -czf "…"`, `7z a "…"` and
// `printf … > "….sha256"` lines out of it, rather than restating the naming as
// a hardcoded fixture. A fixture would still be green after someone renamed an
// asset in the workflow, which is precisely the bug this exists to catch.
//
// `#[cfg(test)]` because none of it is reachable from production — the parser
// exists to read a file that only a test has any business reading.
#[cfg(test)]
mod release_workflow_contract {
    use super::*;

    //
    // `.github/workflows/release.yml` decides the names of the assets we download;
    // this module decides which of those names we will accept. Nothing in the type
    // system joins them, so the only thing that has ever stopped them drifting
    // apart is a test that reads the real workflow. This is that test.
    //
    // It reads the YAML as TEXT and parses the `tar -czf "…"`, `7z a "…"` and
    // `printf … > "….sha256"` lines out of it, rather than restating the naming as
    // a hardcoded fixture. A fixture would still be green after someone renamed an
    // asset in the workflow, which is precisely the bug this exists to catch.

    /// The release workflow as it actually exists in this repository.
    ///
    /// `include_str!` rather than a runtime read so a workflow that fails to parse
    /// (or a path that moves) breaks the build instead of the release, and so the
    /// checked-in copy is exactly the file that runs.
    const RELEASE_WORKFLOW: &str = include_str!("../../../.github/workflows/release.yml");

    /// The tag used to expand a template. [`is_checksum_for`] is a pure string
    /// comparison, so the test is about the SHAPE of the asset name; any version
    /// that looks like the ones we tag releases with does.
    const SAMPLE_TAG: &str = "v0.2.0";

    /// Expands the two shell variables the workflow's Package step uses.
    ///
    /// `$TAG`/`${TAG}` and `${{ matrix.suffix }}` are the only substitutions in the
    /// asset names. A template containing anything else would be left with its
    /// placeholders intact, so [`every_published_asset_has_a_sidecar_the_updater_accepts`]
    /// would fail loudly rather than compare two unexpanded strings for equality.
    fn expand(template: &str, suffix: &str) -> String {
        template
            .replace("${TAG}", SAMPLE_TAG)
            .replace("$TAG", SAMPLE_TAG)
            .replace("${{ matrix.suffix }}", suffix)
            .replace("${matrix.suffix}", suffix)
    }

    /// The first `"…"` token on `line`, if the line starts with `marker`.
    fn first_quoted_after<'a>(line: &'a str, marker: &str) -> Option<&'a str> {
        let rest = line.trim().strip_prefix(marker)?;
        let rest = rest.strip_prefix('"')?;
        let end = rest.find('"')?;
        Some(&rest[..end])
    }

    /// Every `suffix:` value in the build matrix — `linux-x86_64`, `windows-x86_64`,
    /// … The job's `name:` line also mentions `matrix.suffix`, but not in the
    /// `suffix: value` form this looks for.
    fn matrix_suffixes() -> Vec<String> {
        RELEASE_WORKFLOW
            .lines()
            .filter_map(|line| line.trim().strip_prefix("suffix:"))
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .collect()
    }

    /// Splits a matrix suffix into the `(os, arch)` the updater matches on, plus the
    /// archive container that platform ships.
    fn split_suffix(suffix: &str) -> (&str, &str, &str) {
        let (os, arch) = suffix
            .rsplit_once('-')
            .unwrap_or_else(|| panic!("matrix suffix {suffix:?} is not '<os>-<arch>'"));
        let ext = if os == "windows" { "zip" } else { "tar.gz" };
        (os, arch, ext)
    }

    /// The archive filenames the Package step creates: `tar -czf "NAME"` on unix,
    /// `7z a "NAME"` on Windows.
    fn archive_templates() -> Vec<&'static str> {
        RELEASE_WORKFLOW
            .lines()
            .filter_map(|line| {
                first_quoted_after(line, "tar -czf ").or_else(|| first_quoted_after(line, "7z a "))
            })
            .collect()
    }

    /// The archive filename the workflow builds for `suffix`, read out of the
    /// Package step's `case`.
    ///
    /// Both templates sit in the same `case` statement, but only one arm runs per
    /// platform: `$BIN_NAME` ends in `.exe` exactly when the matrix suffix is a
    /// `windows-*` one (release.yml:73-75), and that arm packages a zip. Pairing
    /// every template with every suffix would assert against asset names the
    /// workflow never publishes.
    fn archive_for_suffix(suffix: &str) -> String {
        let (_, _, ext) = split_suffix(suffix);
        let template = archive_templates()
            .into_iter()
            .find(|template| template.ends_with(ext))
            .unwrap_or_else(|| panic!("release.yml publishes no {ext} archive"));
        expand(template, suffix)
    }

    /// The checksum filenames the Package step writes: the right-hand side of a
    /// `printf … > "NAME.sha256"` redirect.
    ///
    /// Scoped to `.sha256` on purpose. An aggregate `checksums.txt` written by the
    /// workflow would be the Go manifest, which [`is_checksum_for`] already accepts
    /// unconditionally; what has to be tied to the workflow is the per-asset sidecar,
    /// because that is the name the two halves used to disagree about.
    fn sidecar_templates() -> Vec<&'static str> {
        RELEASE_WORKFLOW
            .lines()
            .filter_map(|line| line.rsplit_once('>').map(|(_, rhs)| rhs.trim()))
            .filter(|rhs| rhs.starts_with('"') && rhs.ends_with(".sha256\""))
            .map(|rhs| &rhs[1..rhs.len() - 1])
            .collect()
    }

    /// The heart of the contract: for every (platform, archive) pair the workflow
    /// publishes, the sidecar the workflow will emit must be one the updater
    /// accepts — and every platform must actually be covered.
    ///
    /// This is the test that fails if either side moves alone. Narrow
    /// [`is_checksum_for`] back to the extension-retained spelling and it fails on
    /// the stripped form the workflow writes; rename an asset in the workflow and
    /// it fails on the coverage assertion.
    #[test]
    fn every_published_asset_has_a_sidecar_the_updater_accepts() {
        let suffixes = matrix_suffixes();
        let archives = archive_templates();
        let sidecars = sidecar_templates();

        // A parser that silently matches nothing would make every assertion below
        // vacuous and the test permanently green. Refuse to run in that state.
        assert!(
            !suffixes.is_empty(),
            "parsed no matrix suffixes out of release.yml — the parser is broken"
        );
        assert!(
            !archives.is_empty(),
            "parsed no archive names out of release.yml — the parser is broken"
        );
        assert!(
            !sidecars.is_empty(),
            "parsed no `printf … > \"….sha256\"` lines out of release.yml — the parser \
         is broken"
        );

        for suffix in &suffixes {
            let sidecars: Vec<String> = sidecars.iter().map(|name| expand(name, suffix)).collect();
            let archive = archive_for_suffix(suffix);
            assert!(
                sidecars
                    .iter()
                    .any(|sidecar| is_checksum_for(&archive, sidecar)),
                "release.yml publishes `{archive}` and writes {sidecars:?}, none of which \
             is_checksum_for accepts for it"
            );
        }
    }

    /// The other half of the contract, and the one the workflow cannot check for
    /// itself: the updater has to be able to FIND each asset. A sidecar name is
    /// useless if the archive selector rejects the archive next to it.
    #[test]
    fn the_updater_can_select_every_archive_the_workflow_publishes() {
        for suffix in matrix_suffixes() {
            let (os, arch, ext) = split_suffix(&suffix);
            let name = archive_for_suffix(&suffix);
            assert!(
                matches_asset(&name, os, arch, ext),
                "release.yml publishes `{name}` for {os}/{arch}, which matches_asset \
             rejects — `treehouse update` would never find it"
            );
        }
    }

    /// A sidecar that passed [`matches_asset`] would be downloaded *as* the binary
    /// and unpacked as one, so the archive selector has to reject every name the
    /// workflow hands it.
    ///
    /// Checked per platform rather than only for the running one: this is the same
    /// string-vs-string comparison on every machine, and a Linux CI run is the only
    /// place the Windows asset's spelling gets exercised here.
    #[test]
    fn no_sidecar_the_workflow_writes_can_be_mistaken_for_an_archive() {
        for suffix in matrix_suffixes() {
            let (os, arch, ext) = split_suffix(&suffix);
            for template in sidecar_templates() {
                let name = expand(template, &suffix);
                assert!(
                    !matches_asset(&name, os, arch, ext),
                    "sidecar `{name}` matches the {os}/{arch} archive selector"
                );
            }
        }
    }

    /// The specific mismatch this module was written for, spelled out so a future
    /// reader does not have to reconstruct it from a diff: the workflow drops the
    /// container extension, the updater used to keep it, and no release had both.
    #[test]
    fn is_checksum_for_accepts_both_sidecar_spellings() {
        let archive = "treehouse-v0.2.0-macos-aarch64.tar.gz";
        // What release.yml's Package step writes for the above archive.
        assert!(is_checksum_for(
            archive,
            "treehouse-v0.2.0-macos-aarch64.sha256"
        ));
        // The conventional spelling, kept working for hand-uploaded releases.
        assert!(is_checksum_for(
            archive,
            "treehouse-v0.2.0-macos-aarch64.tar.gz.sha256"
        ));
        // Go's aggregate manifest.
        assert!(is_checksum_for(archive, AGGREGATE_CHECKSUM_FILE));
        // The Windows container behaves the same way.
        assert!(is_checksum_for(
            "treehouse-v0.2.0-windows-x86_64.zip",
            "treehouse-v0.2.0-windows-x86_64.sha256"
        ));

        // Fail closed on everything else: another platform's sidecar, the archive
        // itself, a near-miss extension, and an empty name. A wrong checksum file
        // that "looks close enough" would either fail the update on a valid release
        // or — worse — verify against the wrong hash.
        assert!(!is_checksum_for(
            archive,
            "treehouse-v0.2.0-linux-x86_64.sha256"
        ));
        assert!(!is_checksum_for(archive, archive));
        assert!(!is_checksum_for(
            archive,
            "treehouse-v0.2.0-macos-aarch64.sha256.txt"
        ));
        assert!(!is_checksum_for(archive, ""));
    }

    /// Ordering is part of the contract: a release carrying both spellings must
    /// resolve the same way on every run rather than in API order.
    #[test]
    fn the_exact_spelling_is_preferred_over_the_stripped_one() {
        let names = checksum_sidecar_names("treehouse-v0.2.0-macos-aarch64.tar.gz");
        assert_eq!(
            names,
            vec![
                "treehouse-v0.2.0-macos-aarch64.tar.gz.sha256",
                "treehouse-v0.2.0-macos-aarch64.sha256",
            ]
        );
    }

    /// `strip_archive_ext` must not strip a bare extension down to nothing, which
    /// would make `<empty>.sha256` a match for every asset.
    #[test]
    fn strip_archive_ext_refuses_a_name_that_is_only_an_extension() {
        assert_eq!(strip_archive_ext(".sha256"), None);
        assert_eq!(strip_archive_ext(".gz"), None);
        assert_eq!(strip_archive_ext("treehouse.tar.gz"), Some("treehouse"));
        assert_eq!(strip_archive_ext("treehouse.zip"), Some("treehouse"));
        assert_eq!(strip_archive_ext("treehouse"), None);
    }

    /// A `file://` URL for `path`, spelled the way curl and JSON both want it.
    ///
    /// `Path::display` renders `\` as the separator on Windows, and a backslash is
    /// not a legal JSON escape - a body built by interpolating a Windows path does
    /// not parse at all, so the fetch fails before the contract below is ever
    /// checked. The same string is handed to curl, which wants
    /// `file://C:/dir/name`, not `file://C:\dir\name`. Forward slashes are what
    /// `file://` means on every platform, so normalize once here and build every
    /// URL in this test from it - including the value the final assertion compares
    /// against, so the two cannot end up disagreeing about separators either.
    fn file_url(path: &Path) -> String {
        format!("file://{}", path.display().to_string().replace('\\', "/"))
    }

    /// End-to-end over the names the workflow really publishes: the release JSON is
    /// built from `expand`ed workflow templates, so this exercises the whole
    /// lookup — archive selection, sidecar selection, and the `CheckResult` the CLI
    /// hands to `apply` — with no name written by hand.
    #[test]
    fn check_latest_result_finds_the_workflows_own_sidecar_name() {
        let suffix = format!(
            "{}-{}",
            if cfg!(target_os = "macos") {
                "macos"
            } else {
                std::env::consts::OS
            },
            std::env::consts::ARCH
        );
        let asset = archive_for_suffix(&suffix);
        let sidecar = expand(
            sidecar_templates()
                .first()
                .expect("release.yml must write a per-asset sidecar"),
            &suffix,
        );

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(&asset), b"archive bytes").unwrap();
        // The sidecar's CONTENTS are also the workflow's: `printf '%s  %s\n' <hash>
        // <archive>` — the full archive name, extension included.
        std::fs::write(
            dir.path().join(&sidecar),
            format!("{}  {asset}\n", "a".repeat(64)),
        )
        .unwrap();
        let body = format!(
            r#"{{"tag_name":"{SAMPLE_TAG}","assets":[
             {{"name":"{asset}","browser_download_url":"{archive}"}},
             {{"name":"{sidecar}","browser_download_url":"{sidecar_path}"}}]}}"#,
            archive = file_url(&dir.path().join(&asset)),
            sidecar_path = file_url(&dir.path().join(&sidecar)),
        );
        let api = dir.path().join("latest.json");
        std::fs::write(&api, body).unwrap();

        let result = check_latest_result(&file_url(&api), "0.0.1", false)
            .expect("the release fixture must resolve");

        assert_eq!(result.asset_name.as_deref(), Some(asset.as_str()));
        assert_eq!(
            result.checksum_url.as_deref(),
            Some(file_url(&dir.path().join(&sidecar)).as_str()),
            "`treehouse update` needs a checksum URL; the workflow's stripped sidecar \
         name must resolve to one"
        );
        assert!(result.update_available, "{SAMPLE_TAG} is newer than 0.0.1");
    }
}
