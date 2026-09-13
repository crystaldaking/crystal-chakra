//! Bounded GitHub release update checks (issue #204, ADR-0054).
//!
//! A manual `chakra update --check` reports the newest stable release, and
//! `chakra serve` performs at most one gated background check per state
//! directory per 24 hours. All network access stays in this CLI-boundary
//! module: no repository data is sent, offline operation is unaffected, and
//! nothing here ever writes to MCP stdout.

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// GitHub API base for production checks; tests substitute a local stub.
pub const GITHUB_API_BASE: &str = "https://api.github.com";
/// Whole-request deadline for one check (ADR-0054).
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
/// Overall deadline of the background automatic task.
pub const AUTOMATIC_DEADLINE: Duration = Duration::from_secs(10);
/// Response body cap; release metadata is far smaller.
pub const MAX_RESPONSE_BYTES: u64 = 256 * 1024;
/// Minimum interval between automatic checks.
pub const AUTOMATIC_INTERVAL_SECS: u64 = 24 * 60 * 60;
/// Backoff multiplier cap for consecutive failures (2^2 = 4x).
const MAX_BACKOFF_SHIFT: u32 = 2;
/// Environment switch that disables automatic checks completely.
pub const DISABLE_ENV: &str = "CHAKRA_UPDATE_CHECK";
/// Environment override for the state directory.
pub const STATE_DIR_ENV: &str = "CHAKRA_STATE_DIR";

/// Exit status: installed build is current (or newer) — ADR-0054 contract.
pub const EXIT_CURRENT: u8 = 0;
/// Exit status: the check failed or no upgrade can be proposed.
pub const EXIT_FAILED: u8 = 1;
/// Exit status: a newer stable release with a matching platform asset exists.
pub const EXIT_AVAILABLE: u8 = 2;

/// One parsed `vMAJOR.MINOR.PATCH` release version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct ReleaseVersion {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
}

impl fmt::Display for ReleaseVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// Parse a strict `vX.Y.Z` (or `X.Y.Z`) tag. Anything else — prerelease
/// suffixes, missing components, non-numeric text — is malformed by policy
/// and returns `None` (ADR-0054).
pub fn parse_release_version(tag: &str) -> Option<ReleaseVersion> {
    let tag = tag.strip_prefix('v').unwrap_or(tag);
    let mut parts = tag.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some(ReleaseVersion {
        major,
        minor,
        patch,
    })
}

/// The version of this binary.
pub fn current_version() -> ReleaseVersion {
    parse_release_version(env!("CARGO_PKG_VERSION")).unwrap_or(ReleaseVersion {
        major: 0,
        minor: 0,
        patch: 0,
    })
}

/// The release-matrix target triple of this build, if it is a supported
/// release platform.
pub fn platform_target() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Some("x86_64-unknown-linux-gnu"),
        ("macos", "aarch64") => Some("aarch64-apple-darwin"),
        ("macos", "x86_64") => Some("x86_64-apple-darwin"),
        ("windows", "x86_64") => Some("x86_64-pc-windows-msvc"),
        _ => None,
    }
}

/// Latest stable release metadata relevant to an update decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseInfo {
    pub tag: String,
    pub version: ReleaseVersion,
    pub notes_url: String,
    pub assets: Vec<String>,
}

impl ReleaseInfo {
    /// The archive name for `target`, if this release ships one.
    pub fn asset_for(&self, target: &str) -> Option<&str> {
        let archive = if target.ends_with("msvc") {
            format!("chakra-{}-{target}.zip", self.tag)
        } else {
            format!("chakra-{}-{target}.tar.gz", self.tag)
        };
        self.assets
            .iter()
            .find(|name| name.as_str() == archive.as_str())
            .map(String::as_str)
    }
}

/// A failed update check. The manual command prints this as its own clear
/// explanation; the automatic task only logs it (ADR-0054).
#[derive(Debug)]
pub enum CheckError {
    /// Transport, TLS, DNS, timeout, or HTTP-status failure.
    Http(String),
    /// The response was not the expected release document.
    Malformed(String),
    /// The response exceeded the byte cap.
    TooLarge,
}

impl fmt::Display for CheckError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CheckError::Http(message) => write!(f, "release check request failed: {message}"),
            CheckError::Malformed(message) => {
                write!(f, "unexpected release metadata: {message}")
            }
            CheckError::TooLarge => write!(
                f,
                "release metadata exceeded the {MAX_RESPONSE_BYTES}-byte limit"
            ),
        }
    }
}

impl std::error::Error for CheckError {}

#[derive(Deserialize)]
struct ApiAsset {
    name: String,
}

#[derive(Deserialize)]
struct ApiRelease {
    tag_name: String,
    html_url: String,
    assets: Vec<ApiAsset>,
}

/// Fetch the latest stable release. `releases/latest` excludes drafts and
/// prereleases at the API level (ADR-0054).
pub fn fetch_latest_release(api_base: &str) -> Result<ReleaseInfo, CheckError> {
    let url = format!("{api_base}/repos/crystaldaking/crystal-chakra/releases/latest");
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(REQUEST_TIMEOUT))
        .max_redirects(3)
        .build()
        .into();
    let mut response = agent
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", concat!("chakra/", env!("CARGO_PKG_VERSION")))
        .call()
        .map_err(|error| CheckError::Http(error.to_string()))?;
    let body = response
        .body_mut()
        .with_config()
        .limit(MAX_RESPONSE_BYTES)
        .read_to_string()
        .map_err(|error| match error {
            ureq::Error::BodyExceedsLimit(_) => CheckError::TooLarge,
            other => CheckError::Http(other.to_string()),
        })?;
    let parsed: ApiRelease =
        serde_json::from_str(&body).map_err(|error| CheckError::Malformed(error.to_string()))?;
    let version = parse_release_version(&parsed.tag_name)
        .ok_or_else(|| CheckError::Malformed(format!("unparseable tag {}", parsed.tag_name)))?;
    Ok(ReleaseInfo {
        tag: parsed.tag_name,
        version,
        notes_url: parsed.html_url,
        assets: parsed.assets.into_iter().map(|asset| asset.name).collect(),
    })
}

/// True when the environment switch disables automatic checks completely.
pub fn env_disables_automatic(value: Option<&str>) -> bool {
    value
        .map(|value| matches!(value.to_ascii_lowercase().as_str(), "0" | "false" | "off"))
        .unwrap_or(false)
}

/// Resolve the automatic-check state directory, honoring the explicit
/// override and falling back to the platform state directory. `None` means
/// no usable directory: the automatic check stays silent (ADR-0054).
pub fn state_dir() -> Option<PathBuf> {
    if let Some(override_dir) = std::env::var_os(STATE_DIR_ENV) {
        return Some(PathBuf::from(override_dir));
    }
    #[cfg(target_os = "windows")]
    {
        std::env::var_os("LOCALAPPDATA").map(|base| PathBuf::from(base).join("chakra"))
    }
    #[cfg(target_os = "macos")]
    {
        std::env::var_os("HOME").map(|home| {
            PathBuf::from(home)
                .join("Library")
                .join("Application Support")
                .join("chakra")
        })
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state"))
            })
            .map(|base| base.join("chakra"))
    }
}

/// Persisted automatic-check state (ADR-0054 interval and backoff).
#[derive(Debug, Default, Serialize, Deserialize)]
struct AutoState {
    last_attempt_unix: Option<u64>,
    consecutive_failures: u32,
}

fn state_path(state_dir: &Path) -> PathBuf {
    state_dir.join("update-check.json")
}

fn read_state(state_dir: &Path) -> AutoState {
    fs::read_to_string(state_path(state_dir))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

fn write_state(state_dir: &Path, state: &AutoState) -> Result<(), std::io::Error> {
    fs::create_dir_all(state_dir)?;
    let text = serde_json::to_string(state).unwrap_or_else(|_| "{}".to_owned());
    // Atomic publish: a concurrent reader sees the old or the new file,
    // never a partial write.
    let temporary = state_dir.join("update-check.json.tmp");
    fs::write(&temporary, text)?;
    fs::rename(&temporary, state_path(state_dir))
}

/// The backoff interval for a failure count, capped at 4x (ADR-0054).
fn effective_interval(failures: u32) -> u64 {
    AUTOMATIC_INTERVAL_SECS * (1_u64 << failures.min(MAX_BACKOFF_SHIFT))
}

/// Outcome of one automatic-check pass, visible to tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoOutcome {
    /// Opted out, no state directory, or the interval has not elapsed; zero
    /// requests were made.
    Skipped,
    /// A check ran and the installed build is current.
    Current,
    /// A check ran and found a newer stable release with a platform asset.
    Available,
    /// A check was attempted and failed.
    Failed,
}

/// Run one gated automatic pass (blocking). Records the attempt before the
/// request so concurrent process starts cannot storm the API, and applies
/// failure backoff. Never panics and never blocks longer than the request
/// timeout; the caller adds the overall deadline.
pub fn run_automatic_once(
    state_dir: &Path,
    api_base: &str,
    disabled: bool,
    now_unix: u64,
) -> AutoOutcome {
    if disabled {
        return AutoOutcome::Skipped;
    }
    let state = read_state(state_dir);
    if let Some(last) = state.last_attempt_unix {
        let due = last.saturating_add(effective_interval(state.consecutive_failures));
        if now_unix < due {
            return AutoOutcome::Skipped;
        }
    }
    // Gate first, request second: a crashed process still recorded the
    // attempt, so a crash loop cannot produce a request storm.
    let _ = write_state(
        state_dir,
        &AutoState {
            last_attempt_unix: Some(now_unix),
            consecutive_failures: state.consecutive_failures,
        },
    );
    let outcome = match fetch_latest_release(api_base) {
        Ok(info) => {
            let current = current_version();
            if info.version > current {
                match platform_target().and_then(|target| info.asset_for(target)) {
                    Some(asset) => {
                        tracing::info!(
                            installed = %current,
                            latest = %info.tag,
                            url = %info.notes_url,
                            asset,
                            "a newer Chakra release is available; upgrade with the documented installer"
                        );
                        AutoOutcome::Available
                    }
                    None => {
                        tracing::info!(
                            installed = %current,
                            latest = %info.tag,
                            "a newer Chakra release exists but ships no asset for this platform"
                        );
                        AutoOutcome::Current
                    }
                }
            } else {
                AutoOutcome::Current
            }
        }
        Err(error) => {
            tracing::debug!("automatic update check failed: {error}");
            AutoOutcome::Failed
        }
    };
    let _ = write_state(
        state_dir,
        &AutoState {
            last_attempt_unix: Some(now_unix),
            consecutive_failures: match outcome {
                AutoOutcome::Failed => state.consecutive_failures.saturating_add(1),
                _ => 0,
            },
        },
    );
    outcome
}

/// Spawn the bounded automatic background check for `chakra serve`.
///
/// Returns the owned task handle so the caller can abort it on shutdown.
/// The task performs at most one HTTP request, finishes well inside
/// `AUTOMATIC_DEADLINE`, writes nothing to stdout, and never touches query
/// paths. Disabled configuration, an opted-out environment, or an
/// unavailable state directory yield no task and no request.
pub fn spawn_automatic_check(update_automatic: bool) -> Option<tokio::task::JoinHandle<()>> {
    let disabled =
        !update_automatic || env_disables_automatic(std::env::var(DISABLE_ENV).ok().as_deref());
    let state_dir = state_dir()?;
    if disabled {
        return None;
    }
    Some(tokio::spawn(async move {
        let _ = tokio::time::timeout(
            AUTOMATIC_DEADLINE,
            tokio::task::spawn_blocking(move || {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|duration| duration.as_secs())
                    .unwrap_or(0);
                run_automatic_once(&state_dir, GITHUB_API_BASE, false, now);
            }),
        )
        .await;
    }))
}

/// Run the manual `chakra update --check` command (ADR-0054 exit contract).
pub fn run_manual_check(api_base: &str) -> u8 {
    let current = current_version();
    match fetch_latest_release(api_base) {
        Ok(info) => {
            println!("installed: {current}");
            println!("latest stable: {} ({})", info.tag, info.notes_url);
            if info.version <= current {
                println!("chakra is up to date");
                return EXIT_CURRENT;
            }
            match platform_target() {
                Some(target) => match info.asset_for(target) {
                    Some(asset) => {
                        println!("update available: {current} -> {info}", info = info.tag);
                        println!("platform asset: {asset}");
                        println!(
                            "upgrade: run the documented installer (README, Install section) for {}",
                            info.tag
                        );
                        EXIT_AVAILABLE
                    }
                    None => {
                        println!(
                            "release {} ships no asset for {target}; no upgrade can be proposed",
                            info.tag
                        );
                        EXIT_FAILED
                    }
                },
                None => {
                    println!(
                        "this platform ({}-{}) is not in the release matrix; build from source to upgrade",
                        std::env::consts::OS,
                        std::env::consts::ARCH
                    );
                    EXIT_FAILED
                }
            }
        }
        Err(error) => {
            println!("update check unavailable: {error}");
            println!(
                "chakra keeps working offline; try again later or check the releases page manually"
            );
            EXIT_FAILED
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    #[test]
    fn release_versions_parse_and_order_numerically() -> TestResult {
        let nine = parse_release_version("v0.4.9").ok_or("0.4.9 parses")?;
        let ten = parse_release_version("v0.4.10").ok_or("0.4.10 parses")?;
        assert!(ten > nine, "0.4.10 sorts after 0.4.9");
        assert_eq!(parse_release_version("0.4.10"), Some(ten));
        assert_eq!(parse_release_version("v0.4.10-alpha"), None);
        assert_eq!(parse_release_version("v0.4"), None);
        assert_eq!(parse_release_version("v0.4.10.1"), None);
        assert_eq!(parse_release_version("latest"), None);
        assert_eq!(parse_release_version("v0.4.x"), None);
        Ok(())
    }

    /// Serve `responses` in order over one-shot HTTP, counting hits.
    fn stub_server(
        responses: Vec<(u16, String)>,
    ) -> Result<(String, Arc<AtomicUsize>), Box<dyn std::error::Error>> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let port = listener.local_addr()?.port();
        let hits = Arc::new(AtomicUsize::new(0));
        let thread_hits = hits.clone();
        std::thread::spawn(move || {
            for (status, body) in responses {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                thread_hits.fetch_add(1, Ordering::SeqCst);
                let mut buffer = [0_u8; 4096];
                let _ = stream.read(&mut buffer);
                let response = format!(
                    "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
            }
        });
        Ok((format!("http://127.0.0.1:{port}"), hits))
    }

    fn release_json(tag: &str, assets: &[&str]) -> String {
        let assets = assets
            .iter()
            .map(|name| format!("{{\"name\":\"{name}\"}}"))
            .collect::<Vec<_>>()
            .join(",");
        format!(
            "{{\"tag_name\":\"{tag}\",\"html_url\":\"https://example.test/{tag}\",\"assets\":[{assets}]}}"
        )
    }

    #[test]
    fn fetch_parses_latest_release_and_finds_platform_asset() -> TestResult {
        let target = platform_target().ok_or("test platform must be in the matrix")?;
        let archive = if target.ends_with("msvc") {
            format!("chakra-v99.0.0-{target}.zip")
        } else {
            format!("chakra-v99.0.0-{target}.tar.gz")
        };
        let (base, hits) = stub_server(vec![(
            200,
            release_json("v99.0.0", &[&archive, "SHA256SUMS"]),
        )])?;
        let info = fetch_latest_release(&base)?;
        assert_eq!(
            info.version,
            ReleaseVersion {
                major: 99,
                minor: 0,
                patch: 0
            }
        );
        assert_eq!(info.asset_for(target), Some(archive.as_str()));
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        Ok(())
    }

    #[test]
    fn fetch_reports_http_and_malformed_failures() -> TestResult {
        let (base, _) = stub_server(vec![(500, "boom".to_owned())])?;
        let error = fetch_latest_release(&base).err().ok_or("500 must fail")?;
        assert!(matches!(error, CheckError::Http(_)), "{error}");

        let (base, _) = stub_server(vec![(200, "not json".to_owned())])?;
        let error = fetch_latest_release(&base)
            .err()
            .ok_or("bad JSON must fail")?;
        assert!(matches!(error, CheckError::Malformed(_)), "{error}");

        let (base, _) = stub_server(vec![(200, release_json("nightly", &[]))])?;
        let error = fetch_latest_release(&base)
            .err()
            .ok_or("bad tag must fail")?;
        assert!(matches!(error, CheckError::Malformed(_)), "{error}");
        Ok(())
    }

    #[test]
    fn fetch_rejects_oversized_responses() -> TestResult {
        let big = " ".repeat((MAX_RESPONSE_BYTES + 4096) as usize);
        let (base, _) = stub_server(vec![(200, big)])?;
        let error = fetch_latest_release(&base)
            .err()
            .ok_or("oversized must fail")?;
        assert!(matches!(error, CheckError::TooLarge), "{error}");
        Ok(())
    }

    #[test]
    fn env_switch_disables_automatic_checks() {
        assert!(env_disables_automatic(Some("0")));
        assert!(env_disables_automatic(Some("false")));
        assert!(env_disables_automatic(Some("OFF")));
        assert!(!env_disables_automatic(Some("1")));
        assert!(!env_disables_automatic(None));
    }

    #[test]
    fn automatic_gate_enforces_interval_and_backoff() -> TestResult {
        let directory = tempfile::tempdir()?;
        let (base, hits) = stub_server(vec![
            (200, release_json("v0.0.1", &[])),
            (200, release_json("v0.0.1", &[])),
        ])?;
        let now = 1_800_000_000_u64;
        // First pass: due (no state), runs one request.
        let outcome = run_automatic_once(directory.path(), &base, false, now);
        assert_eq!(outcome, AutoOutcome::Current);
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        // Second pass inside the interval: skipped, no request.
        let outcome = run_automatic_once(directory.path(), &base, false, now + 60);
        assert_eq!(outcome, AutoOutcome::Skipped);
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        // Disabled: skipped with zero requests even when due.
        let outcome = run_automatic_once(
            directory.path(),
            &base,
            true,
            now + 2 * AUTOMATIC_INTERVAL_SECS,
        );
        assert_eq!(outcome, AutoOutcome::Skipped);
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        // After the interval: due again.
        let outcome = run_automatic_once(
            directory.path(),
            &base,
            false,
            now + 2 * AUTOMATIC_INTERVAL_SECS,
        );
        assert_eq!(outcome, AutoOutcome::Current);
        assert_eq!(hits.load(Ordering::SeqCst), 2);
        Ok(())
    }

    #[test]
    fn automatic_failure_records_backoff() -> TestResult {
        let directory = tempfile::tempdir()?;
        let (base, hits) = stub_server(vec![(500, "boom".to_owned())])?;
        let now = 1_800_000_000_u64;
        let outcome = run_automatic_once(directory.path(), &base, false, now);
        assert_eq!(outcome, AutoOutcome::Failed);
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        // One failure doubles the interval: still gated at 1.5x the base.
        let outcome = run_automatic_once(
            directory.path(),
            &base,
            false,
            now + AUTOMATIC_INTERVAL_SECS + AUTOMATIC_INTERVAL_SECS / 2,
        );
        assert_eq!(outcome, AutoOutcome::Skipped);
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        Ok(())
    }
}
