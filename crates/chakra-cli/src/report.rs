//! Bounded local diagnostic report export (issue #208).
//!
//! A report is built from an explicit allowlist of typed values: doctor
//! findings, platform identity, non-sensitive effective limits, and
//! provider readiness. Free-text fields pass through a sanitizer that
//! replaces known-sensitive values (worktree/home paths, configured
//! executable paths) and then scrubs residual absolute paths, URLs, and
//! token-like strings, so a provider error message cannot smuggle machine
//! paths or secrets past the policy. Nothing is uploaded; the user reviews
//! the file and attaches it manually.

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::config::EffectiveConfig;
use crate::doctor::Finding;

/// Maximum findings retained in one report.
pub const MAX_FINDINGS: usize = 200;
/// Maximum bytes of one free-text field after sanitization.
pub const MAX_FIELD_BYTES: usize = 2048;
/// Maximum serialized report size; findings are dropped to fit.
pub const MAX_REPORT_BYTES: usize = 256 * 1024;

/// Report construction or write failure.
#[derive(Debug)]
pub struct ReportError(pub String);

impl fmt::Display for ReportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ReportError {}

fn error(message: impl Into<String>) -> ReportError {
    ReportError(message.into())
}

/// Value marked honestly as unavailable (never fabricated as zero/ready).
pub const UNAVAILABLE: &str = "unavailable";

/// Layer names without filesystem paths (ADR-0053 layers, sanitized).
fn layer_name(source: &crate::config::ConfigSource) -> &'static str {
    match source {
        crate::config::ConfigSource::Default => "default",
        crate::config::ConfigSource::Shared(_) => "shared",
        crate::config::ConfigSource::Private(_) => "private",
        crate::config::ConfigSource::Cli => "cli",
    }
}

/// Sanitizes free text for the report allowlist.
#[derive(Debug, Default)]
pub struct Sanitizer {
    replacements: Vec<(String, String)>,
}

impl Sanitizer {
    /// Seed with the worktree root and the user's home directory.
    pub fn new(root: &Path) -> Self {
        let mut sanitizer = Sanitizer::default();
        sanitizer.add_replacement(root.to_string_lossy().into_owned(), "<worktree>");
        if let Some(home) = std::env::var_os("HOME") {
            sanitizer.add_replacement(PathBuf::from(home).to_string_lossy().into_owned(), "<home>");
        }
        sanitizer
    }

    /// Replace one exact sensitive value with an opaque label. Values
    /// shorter than 8 characters are ignored so scrubbing cannot corrupt
    /// ordinary prose.
    pub fn add_replacement(&mut self, sensitive: String, label: &str) {
        if sensitive.len() >= 8 {
            self.replacements.push((sensitive, label.to_owned()));
        }
    }

    /// Sanitize one free-text field: exact replacements first, then
    /// residual pattern scrubs, then the byte cap with an explicit marker.
    pub fn sanitize(&self, text: &str) -> String {
        let mut out = text.to_owned();
        for (sensitive, label) in &self.replacements {
            out = out.replace(sensitive.as_str(), label);
        }
        out = scrub_urls(&out);
        out = scrub_windows_paths(&out);
        out = scrub_unix_paths(&out);
        out = scrub_token_like(&out);
        if out.len() > MAX_FIELD_BYTES {
            out.truncate(MAX_FIELD_BYTES);
            out.push_str("…[truncated]");
        }
        out
    }
}

fn scrub_urls(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for word in text.split_inclusive(char::is_whitespace) {
        if word.trim_start().starts_with("http://") || word.trim_start().starts_with("https://") {
            let trailing: String = word
                .chars()
                .rev()
                .take_while(|c| c.is_whitespace())
                .collect::<String>()
                .chars()
                .rev()
                .collect();
            out.push_str("<url>");
            out.push_str(&trailing);
        } else {
            out.push_str(word);
        }
    }
    out
}

fn looks_like_windows_path(word: &str) -> bool {
    let bytes = word.as_bytes();
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/')
}

fn scrub_windows_paths(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for word in text.split_inclusive(char::is_whitespace) {
        if looks_like_windows_path(word.trim_end()) {
            let trailing: String = word
                .chars()
                .rev()
                .take_while(|c| c.is_whitespace())
                .collect::<String>()
                .chars()
                .rev()
                .collect();
            out.push_str("<path>");
            out.push_str(&trailing);
        } else {
            out.push_str(word);
        }
    }
    out
}

/// Scrub residual absolute Unix paths: two or more `/segment` parts with no
/// whitespace. Runs after exact replacements so `<worktree>` labels survive.
fn scrub_unix_paths(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for word in text.split_inclusive(char::is_whitespace) {
        let trimmed = word.trim_end();
        let trailing = &word[trimmed.len()..];
        let core = trimmed.trim_end_matches([')', ']', ',', ';', '.', ':']);
        let suffix = &trimmed[core.len()..];
        let is_path = core.starts_with('/')
            && core.len() > 4
            && core[1..].contains('/')
            && core.chars().all(|c| {
                c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-' | '+' | '~')
            });
        if is_path {
            out.push_str("<path>");
            out.push_str(suffix);
            out.push_str(trailing);
        } else {
            out.push_str(word);
        }
    }
    out
}

/// Scrub long token-like runs (32+ unbroken credential characters).
fn scrub_token_like(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut current = String::new();
    for word in text.split_inclusive(char::is_whitespace) {
        let trimmed = word.trim_end();
        let trailing = &word[trimmed.len()..];
        let core =
            trimmed.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '-' && c != '_');
        if core.len() >= 32
            && core
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            current.push_str("<redacted>");
            current.push_str(trailing);
        } else {
            current.push_str(word);
        }
    }
    out.push_str(&current);
    out
}

#[derive(Clone, Serialize)]
struct PlatformDocument {
    os: &'static str,
    arch: &'static str,
}

#[derive(Clone, Serialize)]
struct WorktreeDocument {
    id: &'static str,
    head_revision: String,
}

#[derive(Clone, Serialize)]
struct ProviderDocument {
    provider: &'static str,
    language: &'static str,
    enabled: bool,
    readiness: &'static str,
    version: String,
}

#[derive(Clone, Serialize)]
struct ReportDocument {
    schema_version: u32,
    kind: &'static str,
    scope: &'static str,
    chakra_version: &'static str,
    platform: PlatformDocument,
    worktree: WorktreeDocument,
    limits: serde_json::Value,
    providers: Vec<ProviderDocument>,
    findings: Vec<Finding>,
    truncated: Vec<String>,
    omissions: Vec<String>,
}

/// One bounded, sanitized report ready for review and manual attachment.
pub struct Report {
    pub json: String,
    pub finding_count: usize,
    pub truncated_sections: Vec<String>,
}

/// Build the report from allowlisted typed values. `probe` enables the
/// bounded version probes from #207; `omit_revision` forces the honest
/// unavailable marker in tests.
pub fn build_report(
    root: &Path,
    effective: Option<&EffectiveConfig>,
    findings: &[Finding],
) -> Result<Report, ReportError> {
    let mut sanitizer = Sanitizer::new(root);
    if let Some(effective) = effective {
        for key in crate::config::ProviderKey::ALL {
            if let Some(path) = &effective.provider(*key).path {
                sanitizer.add_replacement(path.to_string_lossy().into_owned(), "<provider-exe>");
            }
        }
        for source in effective.sources().values() {
            match source {
                crate::config::ConfigSource::Shared(path)
                | crate::config::ConfigSource::Private(path) => {
                    sanitizer.add_replacement(path.to_string_lossy().into_owned(), "<config>");
                }
                _ => {}
            }
        }
    }

    let mut truncated = Vec::new();
    let mut sanitized_findings: Vec<Finding> = findings
        .iter()
        .take(MAX_FINDINGS)
        .map(|finding| {
            let mut sanitized = finding.clone();
            sanitized.subject = sanitizer.sanitize(&finding.subject);
            sanitized.applicability = sanitizer.sanitize(&finding.applicability);
            sanitized.evidence = sanitizer.sanitize(&finding.evidence);
            sanitized.advice = sanitizer.sanitize(&finding.advice);
            sanitized
        })
        .collect();
    if findings.len() > MAX_FINDINGS {
        truncated.push(format!(
            "findings: kept {MAX_FINDINGS} of {}",
            findings.len()
        ));
    }

    let head_revision = chakra_git::resolve_head_commit_with_context(
        root,
        &chakra_domain::operation::OperationContext::unbounded(),
    )
    .ok()
    .flatten()
    .unwrap_or_else(|| UNAVAILABLE.to_owned());

    let limits = match effective {
        Some(effective) => {
            let mut map = serde_json::Map::new();
            for (key, source) in effective.sources() {
                if key.starts_with("providers.") && key.ends_with(".path") {
                    continue; // executable paths never enter the report
                }
                let value = resolve_limit_value(effective, key);
                map.insert(
                    sanitizer.sanitize(key),
                    serde_json::json!({
                        "value": value,
                        "source": layer_name(source),
                    }),
                );
            }
            serde_json::Value::Object(map)
        }
        None => serde_json::Value::String(UNAVAILABLE.to_owned()),
    };

    let providers = crate::analysis::PROVIDER_SPECS
        .iter()
        .map(|spec| {
            let enabled = effective
                .map(|effective| effective.provider(spec.key).enabled)
                .unwrap_or(false);
            let readiness = match effective {
                None => UNAVAILABLE,
                Some(effective) if !effective.provider(spec.key).enabled => "disabled",
                Some(_) => {
                    if crate::doctor::find_on_path(spec.default_executable).is_some() {
                        "installed (isolated observation; session readiness via status)"
                    } else {
                        "not installed"
                    }
                }
            };
            ProviderDocument {
                provider: spec.key.as_str(),
                language: spec.language,
                enabled,
                readiness,
                version: UNAVAILABLE.to_owned(),
            }
        })
        .collect();

    let base = ReportDocument {
        schema_version: 1,
        kind: "chakra-diagnostic-report",
        scope: "isolated-inspection",
        chakra_version: env!("CARGO_PKG_VERSION"),
        platform: PlatformDocument {
            os: std::env::consts::OS,
            arch: std::env::consts::ARCH,
        },
        worktree: WorktreeDocument {
            id: "worktree-1",
            head_revision,
        },
        limits,
        providers,
        findings: Vec::new(),
        truncated: Vec::new(),
        omissions: vec![
            "live session state and counters (query the session status tool)".to_owned(),
            "source contents, snippets, symbol and search text".to_owned(),
            "environment values, credentials, remote URLs, absolute machine paths".to_owned(),
            "raw logs and configuration files".to_owned(),
        ],
    };
    let render = |findings: &[Finding], truncated: &[String]| -> Result<String, ReportError> {
        let mut document = base.clone();
        document.findings = findings.to_vec();
        document.truncated = truncated.to_vec();
        serde_json::to_string_pretty(&document)
            .map_err(|serialize| error(format!("cannot serialize report: {serialize}")))
    };

    let mut json = render(&sanitized_findings, &truncated)?;
    while json.len() > MAX_REPORT_BYTES && !sanitized_findings.is_empty() {
        let dropped = sanitized_findings.len() / 2;
        sanitized_findings.truncate(dropped.max(1));
        truncated = vec![format!(
            "findings: truncated to {} entries to fit the {}-byte report bound",
            sanitized_findings.len(),
            MAX_REPORT_BYTES
        )];
        json = render(&sanitized_findings, &truncated)?;
    }
    if json.len() > MAX_REPORT_BYTES {
        return Err(error(format!(
            "report exceeds the {MAX_REPORT_BYTES}-byte bound even without findings"
        )));
    }
    Ok(Report {
        json,
        finding_count: sanitized_findings.len(),
        truncated_sections: truncated,
    })
}

/// Resolve a dotted limit key to its numeric/boolean value from the
/// effective configuration. Unknown keys are marked unavailable.
fn resolve_limit_value(effective: &EffectiveConfig, key: &str) -> serde_json::Value {
    let budgets = &effective.budgets;
    let value: Option<serde_json::Value> = match key {
        "index.max_files" => Some(budgets.max_files.into()),
        "index.max_source_file_bytes" => Some(budgets.max_source_file_bytes.into()),
        "index.max_workspace_source_bytes" => Some(budgets.max_workspace_source_bytes.into()),
        "index.max_symbols" => Some(budgets.max_symbols.into()),
        "index.max_edges" => Some(budgets.max_edges.into()),
        "index.max_call_sites" => Some(budgets.max_call_sites.into()),
        "index.max_workers" => Some(budgets.max_workers.into()),
        "index.startup_target_millis" => Some(budgets.startup_target_millis.into()),
        "index.memory_target_bytes" => Some(budgets.memory_target_bytes.into()),
        "startup.max_workspaces" => Some((effective.max_workspaces as u64).into()),
        "startup.live_index_startup_timeout_millis" => {
            Some(effective.live_index_startup_timeout_millis.into())
        }
        "providers.max_active" => Some((effective.max_active_providers as u64).into()),
        "providers.max_active_per_workspace" => {
            Some((effective.max_active_providers_per_workspace as u64).into())
        }
        "providers.max_reserved_memory_bytes" => {
            Some(effective.max_provider_reserved_memory_bytes.into())
        }
        "providers.max_reserved_memory_bytes_per_workspace" => Some(
            effective
                .max_provider_reserved_memory_bytes_per_workspace
                .into(),
        ),
        "providers.max_concurrent_queries" => {
            Some((effective.max_concurrent_provider_queries as u64).into())
        }
        "providers.max_queued_queries" => {
            Some((effective.max_queued_provider_queries as u64).into())
        }
        "providers.queue_timeout_millis" => Some(effective.provider_queue_timeout_millis.into()),
        "providers.idle_timeout_millis" => Some(effective.provider_idle_timeout_millis.into()),
        "providers.jdtls_readiness_timeout_millis" => {
            Some(effective.jdtls_readiness_timeout_millis.into())
        }
        "update.automatic" => Some(effective.update_automatic.into()),
        _ => None,
    };
    value.unwrap_or_else(|| serde_json::Value::String(UNAVAILABLE.to_owned()))
}

/// Write the report to `path`: refuse to overwrite without `force`,
/// restrictive permissions where supported, temporary file + rename, and
/// partial-output cleanup on error (issue #208).
pub fn write_report(path: &Path, json: &str, force: bool) -> Result<(), ReportError> {
    if path.exists() && !force {
        return Err(error(format!(
            "{} already exists; pass --force to replace it",
            path.display()
        )));
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|io| error(format!("cannot create {}: {io}", parent.display())))?;
    }
    let temporary = path.with_extension("chakra-report-tmp");
    let write_result = (|| -> Result<(), ReportError> {
        fs::write(&temporary, json)
            .map_err(|io| error(format!("cannot write {}: {io}", path.display())))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600))
                .map_err(|io| error(format!("cannot restrict {}: {io}", path.display())))?;
        }
        fs::rename(&temporary, path)
            .map_err(|io| error(format!("cannot install {}: {io}", path.display())))?;
        Ok(())
    })();
    if write_result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    write_result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doctor::Severity;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn finding_with(evidence: &str) -> Finding {
        Finding::new(
            "provider-readiness",
            Severity::Warning,
            "provider:test".to_owned(),
            evidence.to_owned(),
            "advice".to_owned(),
        )
    }

    #[test]
    fn sanitizer_removes_paths_urls_tokens_and_known_secrets() {
        let root = Path::new("/users/alice/project");
        let mut sanitizer = Sanitizer::new(root);
        sanitizer.add_replacement("ghp_exampletoken1234567890abcdef".to_owned(), "<token>");
        let text = "error at /users/alice/project/src/secret.rs and /etc/other/file.rs; \
                    see https://user:pw@example.test/x for C:\\Users\\alice\\doc.txt \
                    token ghp_exampletoken1234567890abcdef \
                    raw abcdefghijklmnopqrstuvwxyz0123456789abcd";
        let sanitized = sanitizer.sanitize(text);
        assert!(sanitized.contains("<worktree>"), "{sanitized}");
        assert!(!sanitized.contains("/users/alice"), "{sanitized}");
        assert!(!sanitized.contains("https://"), "{sanitized}");
        assert!(!sanitized.contains("C:\\Users"), "{sanitized}");
        assert!(sanitized.contains("<url>"), "{sanitized}");
        assert!(sanitized.contains("<path>"), "{sanitized}");
        assert!(sanitized.contains("<token>"), "{sanitized}");
        assert!(sanitized.contains("<redacted>"), "{sanitized}");
        assert!(
            !sanitized.contains("abcdefghijklmnopqrstuvwxyz0123456789abcd"),
            "{sanitized}"
        );
        // Ordinary prose survives.
        assert!(sanitized.contains("error at"), "{sanitized}");
        assert!(sanitized.contains("secret.rs"), "{sanitized}");
    }

    #[test]
    fn sanitizer_enforces_field_byte_cap() {
        let sanitizer = Sanitizer::default();
        let long = "word ".repeat(MAX_FIELD_BYTES);
        let sanitized = sanitizer.sanitize(&long);
        assert!(sanitized.len() <= MAX_FIELD_BYTES + "[truncated]".len() + 4);
        assert!(sanitized.ends_with("…[truncated]"));
    }

    #[test]
    fn report_contains_no_root_home_or_config_paths() -> TestResult {
        let directory = tempfile::tempdir()?;
        std::fs::write(
            directory.path().join(crate::config::SHARED_CONFIG_FILENAME),
            "schema_version = 1\n\n[index]\nmax_files = 10\n",
        )?;
        let effective = crate::config::ConfigLayers::load(directory.path(), None)?.merge();
        let findings = vec![finding_with(&format!(
            "probe failed near {} with /home/runner/sensitive/dir in the message",
            directory.path().display()
        ))];
        let report = build_report(directory.path(), Some(&effective), &findings)?;
        let json = &report.json;
        assert!(
            !json.contains(&directory.path().to_string_lossy().into_owned()),
            "{json}"
        );
        assert!(!json.contains("/home/runner"), "{json}");
        let parsed: serde_json::Value = serde_json::from_str(json)?;
        assert_eq!(parsed["schema_version"], 1);
        assert_eq!(parsed["kind"], "chakra-diagnostic-report");
        assert_eq!(parsed["scope"], "isolated-inspection");
        assert_eq!(parsed["limits"]["index.max_files"]["value"], 10);
        assert_eq!(parsed["limits"]["index.max_files"]["source"], "shared");
        assert!(
            parsed["limits"]
                .get("providers.rust-analyzer.path")
                .is_none()
        );
        assert!(parsed["providers"].is_array());
        assert!(parsed["omissions"].is_array());
        Ok(())
    }

    #[test]
    fn unavailable_values_are_marked_not_fabricated() -> TestResult {
        let directory = tempfile::tempdir()?;
        let report = build_report(directory.path(), None, &[])?;
        let parsed: serde_json::Value = serde_json::from_str(&report.json)?;
        assert_eq!(parsed["limits"], UNAVAILABLE);
        assert_eq!(parsed["worktree"]["head_revision"], UNAVAILABLE);
        for provider in parsed["providers"].as_array().into_iter().flatten() {
            assert_eq!(provider["version"], UNAVAILABLE);
        }
        Ok(())
    }

    #[test]
    fn write_report_preserves_existing_and_restricts_permissions() -> TestResult {
        let directory = tempfile::tempdir()?;
        let target = directory.path().join("report.json");
        std::fs::write(&target, "original")?;
        let failure = write_report(&target, "{}", false)
            .err()
            .ok_or("existing destination must be preserved without --force")?;
        assert!(failure.to_string().contains("--force"), "{failure}");
        assert_eq!(std::fs::read_to_string(&target)?, "original");
        write_report(&target, "{\"ok\":true}", true)?;
        assert_eq!(std::fs::read_to_string(&target)?, "{\"ok\":true}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&target)?.permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "report is owner-only");
        }
        Ok(())
    }

    #[test]
    fn nested_sensitive_text_in_provider_errors_is_scrubbed() -> TestResult {
        let directory = tempfile::tempdir()?;
        let findings = vec![finding_with(
            "server stderr: thread 'x' panicked at /builds/agent/home/.cargo/registry/src/secret/lib.rs: \
             Authorization: Bearer abcdefghijklmnopqrstuvwxyz0123456789abcd failed",
        )];
        let report = build_report(directory.path(), None, &findings)?;
        assert!(!report.json.contains("/builds/agent"), "{}", report.json);
        assert!(!report.json.contains("Bearer abcdef"), "{}", report.json);
        assert!(report.json.contains("<redacted>"), "{}", report.json);
        Ok(())
    }
}
