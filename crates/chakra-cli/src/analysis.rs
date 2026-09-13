//! Provider and project analysis for `chakra doctor` (issue #207).
//!
//! Everything here is an *isolated inspection*: it observes configuration,
//! the filesystem, `PATH`, and — only in explicit probe mode — bounded
//! `--version` executions. It never watches, indexes, restarts providers,
//! downloads, or builds project tooling, and it never claims to observe the
//! agent's running session. Session-observable states (dormant, catching
//! up, ready for a revision, degraded) are reported as such, not guessed.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::config::{EffectiveConfig, ProviderKey};
use crate::doctor::{Finding, Severity};

/// Version-probe deadline per executable (ADR: bounded optional probes).
pub const PROBE_DEADLINE: Duration = Duration::from_secs(2);
/// Probe output cap.
pub const PROBE_OUTPUT_CAP: u64 = 16 * 1024;

/// Static provider knowledge used by diagnostics. Keep in sync with
/// `docs/languages/*.md`; Kotlin from #209 plugs into the same table.
pub struct ProviderSpec {
    pub key: ProviderKey,
    pub language: &'static str,
    pub default_executable: &'static str,
    /// Arguments that print a version, if the server supports one.
    pub version_args: Option<&'static [&'static str]>,
    /// Human-readable pinned expectation (docs/languages reference).
    pub expected: &'static str,
    /// Acceptable version prefix for compatibility findings; `None` means
    /// compatibility is informational only and never warned on.
    pub expected_prefix: Option<&'static str>,
    /// Language documentation with install guidance.
    pub language_doc: &'static str,
    /// Project metadata markers: `Exact` filenames or `Suffix` extensions.
    pub metadata_markers: &'static [Marker],
}

#[derive(Debug, Clone, Copy)]
pub enum Marker {
    Exact(&'static str),
    Suffix(&'static str),
}

pub const PROVIDER_SPECS: &[ProviderSpec] = &[
    ProviderSpec {
        key: ProviderKey::RustAnalyzer,
        language: "Rust",
        default_executable: "rust-analyzer",
        version_args: Some(&["--version"]),
        expected: "rust-analyzer from the pinned Rust 1.97.1 toolchain",
        expected_prefix: None,
        language_doc: "docs/languages/rust.md",
        metadata_markers: &[Marker::Exact("Cargo.toml")],
    },
    ProviderSpec {
        key: ProviderKey::Vtsls,
        language: "TypeScript/JavaScript",
        default_executable: "vtsls",
        version_args: Some(&["--version"]),
        expected: "vtsls with a resolvable TypeScript (docs/languages/typescript.md)",
        expected_prefix: None,
        language_doc: "docs/languages/typescript.md",
        metadata_markers: &[
            Marker::Exact("package.json"),
            Marker::Exact("tsconfig.json"),
        ],
    },
    ProviderSpec {
        key: ProviderKey::Pyright,
        language: "Python",
        default_executable: "pyright-langserver",
        version_args: Some(&["--version"]),
        expected: "pyright per docs/languages/python.md",
        expected_prefix: None,
        language_doc: "docs/languages/python.md",
        metadata_markers: &[
            Marker::Exact("pyproject.toml"),
            Marker::Exact("setup.py"),
            Marker::Exact("requirements.txt"),
        ],
    },
    ProviderSpec {
        key: ProviderKey::Jdtls,
        language: "Java",
        default_executable: "jdtls",
        version_args: None,
        expected: "jdtls with a JDK 21 runtime (docs/languages/java.md)",
        expected_prefix: None,
        language_doc: "docs/languages/java.md",
        metadata_markers: &[
            Marker::Exact("pom.xml"),
            Marker::Exact("build.gradle"),
            Marker::Exact("build.gradle.kts"),
        ],
    },
    ProviderSpec {
        key: ProviderKey::CsharpLs,
        language: "C#",
        default_executable: "csharp-ls",
        version_args: Some(&["--version"]),
        expected: "csharp-ls 0.26.x on the .NET 10 SDK",
        expected_prefix: Some("0.26"),
        language_doc: "docs/languages/csharp.md",
        metadata_markers: &[Marker::Suffix(".sln"), Marker::Suffix(".csproj")],
    },
    ProviderSpec {
        key: ProviderKey::BashLanguageServer,
        language: "Shell",
        default_executable: "bash-language-server",
        version_args: Some(&["--version"]),
        expected: "bash-language-server 5.6.x",
        expected_prefix: Some("5.6"),
        language_doc: "docs/languages/shell.md",
        metadata_markers: &[Marker::Suffix(".sh")],
    },
    ProviderSpec {
        key: ProviderKey::Clangd,
        language: "C/C++",
        default_executable: "clangd",
        version_args: Some(&["--version"]),
        expected: "clangd 21",
        expected_prefix: Some("21"),
        language_doc: "docs/languages/cpp.md",
        metadata_markers: &[
            Marker::Exact("compile_commands.json"),
            Marker::Exact("CMakeLists.txt"),
            Marker::Suffix(".cpp"),
            Marker::Suffix(".c"),
        ],
    },
    ProviderSpec {
        key: ProviderKey::TerraformLs,
        language: "HCL/Terraform",
        default_executable: "terraform-ls",
        version_args: Some(&["--version"]),
        expected: "terraform-ls 0.39.x",
        expected_prefix: Some("0.39"),
        language_doc: "docs/languages/hcl.md",
        metadata_markers: &[Marker::Suffix(".tf")],
    },
    ProviderSpec {
        key: ProviderKey::Gopls,
        language: "Go",
        default_executable: "gopls",
        version_args: Some(&["version"]),
        expected: "gopls 0.23.x with the Go toolchain",
        expected_prefix: Some("0.23"),
        language_doc: "docs/languages/go.md",
        metadata_markers: &[Marker::Exact("go.mod")],
    },
    ProviderSpec {
        key: ProviderKey::KotlinLsp,
        language: "Kotlin",
        default_executable: "kotlin-lsp",
        version_args: None,
        expected: "kotlin-server 262.9593.0 (Alpha, bin/intellij-server, JDK 25; docs/languages/kotlin.md)",
        expected_prefix: None,
        language_doc: "docs/languages/kotlin.md",
        metadata_markers: &[
            Marker::Exact("settings.gradle.kts"),
            Marker::Exact("settings.gradle"),
            Marker::Exact("pom.xml"),
            Marker::Suffix(".kt"),
        ],
    },
];

/// Look up an executable in explicit `PATH` entries (testable without
/// touching the process environment).
pub fn find_executable_in(executable: &str, path_entries: &[PathBuf]) -> Option<PathBuf> {
    for directory in path_entries {
        let candidate = directory.join(executable);
        if crate::doctor::is_executable_file(&candidate) {
            return Some(candidate);
        }
    }
    None
}

/// Extract the first `X.Y[.Z]` version string from probe output.
pub fn extract_version(output: &str) -> Option<String> {
    let mut start = None;
    let bytes = output.as_bytes();
    for (index, byte) in bytes.iter().enumerate() {
        if byte.is_ascii_digit() {
            start = Some(index);
            break;
        }
        if *byte == b'\n' {
            continue;
        }
    }
    let start = start?;
    let mut end = start;
    while end < bytes.len() && (bytes[end].is_ascii_digit() || bytes[end] == b'.') {
        end += 1;
    }
    let candidate = &output[start..end];
    let candidate = candidate.trim_end_matches('.');
    if candidate.split('.').next()?.parse::<u64>().is_ok() {
        Some(candidate.to_owned())
    } else {
        None
    }
}

/// Outcome of one bounded version probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeOutcome {
    Version(String),
    TimedOut,
    Failed(String),
}

/// Run `<executable> <version-args>` with a hard deadline and output cap,
/// reaping the child on timeout (owned subprocess cleanup).
pub fn probe_version(executable: &Path, args: &[&str]) -> ProbeOutcome {
    let child = std::process::Command::new(executable)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn();
    let mut child = match child {
        Ok(child) => child,
        Err(error) => return ProbeOutcome::Failed(error.to_string()),
    };
    let deadline = Instant::now() + PROBE_DEADLINE;
    loop {
        match child.try_wait() {
            Ok(Some(_status)) => {
                let mut output = String::new();
                if let Some(stdout) = child.stdout.take() {
                    use std::io::Read as _;
                    let _ = stdout.take(PROBE_OUTPUT_CAP).read_to_string(&mut output);
                }
                return match extract_version(&output) {
                    Some(version) => ProbeOutcome::Version(version),
                    None => ProbeOutcome::Failed("no version in output".to_owned()),
                };
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return ProbeOutcome::TimedOut;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return ProbeOutcome::Failed(error.to_string());
            }
        }
    }
}

/// True when any metadata marker exists at the worktree root (one bounded
/// directory listing; no recursive scan).
pub fn project_metadata_present(root: &Path, markers: &[Marker]) -> bool {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(_) => return false,
    };
    let names: Vec<String> = entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    markers.iter().any(|marker| match marker {
        Marker::Exact(name) => names.iter().any(|entry| entry == name),
        Marker::Suffix(suffix) => names.iter().any(|entry| entry.ends_with(suffix)),
    })
}

/// Derive provider findings from configuration and the environment.
/// `probe` enables bounded `--version` executions; `path_entries` overrides
/// `PATH` for tests.
pub fn analyze_providers(
    root: &Path,
    effective: &EffectiveConfig,
    probe: bool,
    path_entries: Option<&[PathBuf]>,
) -> Vec<Finding> {
    let mut findings = Vec::new();
    for spec in PROVIDER_SPECS {
        let subject = format!("provider:{}", spec.key.as_str());
        let settings = effective.provider(spec.key);
        if !settings.enabled {
            findings.push(
                Finding::info(
                    "provider-readiness",
                    subject,
                    format!(
                        "{} precise enrichment is disabled; syntax-only operation for {} is intentional and healthy",
                        spec.key.as_str(),
                        spec.language
                    ),
                )
                .with_applicability(spec.language),
            );
            continue;
        }
        // A default-sourced path (for example rust-analyzer's PATH name) is
        // discovery input, not a configured override; only private/CLI
        // layers pin an executable (ADR-0053 sources).
        let source_key = format!("providers.{}.path", spec.key.as_str());
        let configured_path = match effective.sources().get(source_key.as_str()) {
            Some(crate::config::ConfigSource::Private(_))
            | Some(crate::config::ConfigSource::Cli) => settings.path.clone(),
            _ => None,
        };
        let executable = match &configured_path {
            Some(configured) => {
                if crate::doctor::is_executable_file(configured) {
                    Some(configured.clone())
                } else {
                    findings.push(
                        Finding::error(
                            "provider-readiness",
                            subject.clone(),
                            format!(
                                "configured executable {} does not exist or is not executable",
                                configured.display()
                            ),
                            format!(
                                "fix providers.{}.path in chakra.local.toml or the CLI override",
                                spec.key.as_str()
                            ),
                        )
                        .with_applicability(spec.language),
                    );
                    None
                }
            }
            None => match path_entries {
                Some(entries) => find_executable_in(spec.default_executable, entries),
                None => crate::doctor::find_on_path(spec.default_executable),
            },
        };
        let Some(executable) = executable else {
            if configured_path.is_some() {
                // Misconfigured case already reported.
                continue;
            }
            findings.push(
                Finding::warning(
                    "provider-readiness",
                    subject.clone(),
                    format!(
                        "{} is enabled but `{}` was not found on PATH; queries for {} degrade to syntax honestly",
                        spec.key.as_str(),
                        spec.default_executable,
                        spec.language
                    ),
                    format!(
                        "install {} (see {}) or disable it with providers.{}.enabled = false",
                        spec.expected, spec.language_doc, spec.key.as_str()
                    ),
                )
                .with_applicability(spec.language),
            );
            continue;
        };
        let mut evidence = format!(
            "{} found at {}; activation, revision readiness, and degradation are session-observable through the status tool",
            spec.key.as_str(),
            executable.display()
        );
        let mut severity = Severity::Info;
        let mut advice = String::new();
        if !project_metadata_present(root, spec.metadata_markers) {
            severity = Severity::Warning;
            evidence.push_str(&format!(
                "; no {} project metadata found at the worktree root, so precise project import will likely fail or degrade",
                spec.language
            ));
            advice = format!(
                "open the worktree that contains the {} project files, or accept syntax-only facts for {}",
                spec.language, spec.language
            );
        }
        if probe {
            match (spec.version_args, spec.expected_prefix) {
                (Some(args), prefix) => match probe_version(&executable, args) {
                    ProbeOutcome::Version(version) => {
                        evidence.push_str(&format!(
                            "; probe (isolated, bounded) reports version {version}"
                        ));
                        if let Some(prefix) = prefix
                            && !version.starts_with(prefix)
                        {
                            severity = Severity::Warning;
                            evidence.push_str(&format!(
                                "; expected {} but found {version}",
                                spec.expected
                            ));
                            advice = format!(
                                "install the pinned server version (see {}); an incompatible server degrades to honest syntax fallback",
                                spec.language_doc
                            );
                        }
                    }
                    ProbeOutcome::TimedOut => {
                        severity = Severity::Warning;
                        evidence.push_str("; version probe timed out (bounded, child reaped)");
                        advice = format!(
                            "check that {} starts correctly; see {}",
                            executable.display(),
                            spec.language_doc
                        );
                    }
                    ProbeOutcome::Failed(reason) => {
                        severity = Severity::Warning;
                        evidence.push_str(&format!("; version probe failed: {reason}"));
                        advice = format!("check the executable; see {}", spec.language_doc);
                    }
                },
                (None, _) => {
                    evidence.push_str("; this server exposes no version probe");
                }
            }
        } else {
            evidence.push_str("; run doctor --probe for a bounded version/compatibility check");
        }
        findings.push(
            Finding::new("provider-readiness", severity, subject, evidence, advice)
                .with_applicability(spec.language),
        );
    }
    findings
}

/// Compare the worktree's tracked source inventory with the configured
/// index budget, using the same Git-aware discovery indexing uses.
pub fn analyze_index_budget(root: &Path, effective: &EffectiveConfig) -> Option<Finding> {
    let files = chakra_git::discover_source_files(root).ok()?;
    let count = files.len() as u64;
    if count <= effective.budgets.max_files {
        return Some(Finding::info(
            "index-budget",
            "project".to_owned(),
            format!(
                "{count} tracked supported source files fit index.max_files = {}",
                effective.budgets.max_files
            ),
        ));
    }
    Some(Finding::warning(
        "index-budget",
        "project".to_owned(),
        format!(
            "{count} tracked supported source files exceed index.max_files = {}; indexing will bound and drop files",
            effective.budgets.max_files
        ),
        "raise index.max_files in chakra.toml, narrow the worktree, or accept bounded indexing"
            .to_owned(),
    ))
}

/// The session-vs-isolation explanation, emitted once per run so an
/// isolated report can never be mistaken for the agent's live session.
pub fn isolation_note() -> Finding {
    Finding::info(
        "diagnostic-scope",
        "session".to_owned(),
        "this is an isolated inspection of configuration and environment, not the agent's running session; dormant/catching-up/ready/degraded states and live counters are observable through the Chakra status tool in the session".to_owned(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ConfigLayers;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn effective(
        contents: Option<&str>,
    ) -> Result<(tempfile::TempDir, EffectiveConfig), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        if let Some(contents) = contents {
            std::fs::write(
                directory.path().join(crate::config::SHARED_CONFIG_FILENAME),
                contents,
            )?;
        }
        let effective = ConfigLayers::load(directory.path(), None)?.merge();
        Ok((directory, effective))
    }

    #[test]
    fn extract_version_finds_semver_in_noise() {
        assert_eq!(
            extract_version("rust-analyzer 1.97.1 (abc 2026)"),
            Some("1.97.1".to_owned())
        );
        assert_eq!(extract_version("gopls v0.23.1"), Some("0.23.1".to_owned()));
        assert_eq!(
            extract_version("clangd version 21.1.0"),
            Some("21.1.0".to_owned())
        );
        assert_eq!(extract_version("no digits here"), None);
    }

    #[test]
    fn disabled_provider_is_healthy_syntax_only() -> TestResult {
        let (directory, effective) = effective(Some(
            "schema_version = 1\n\n[providers.gopls]\nenabled = false\n",
        ))?;
        let findings = analyze_providers(directory.path(), &effective, false, Some(&[]));
        let gopls = findings
            .iter()
            .find(|finding| finding.subject == "provider:gopls")
            .ok_or("gopls finding")?;
        assert_eq!(gopls.severity, Severity::Info, "{gopls:?}");
        assert!(gopls.evidence.contains("disabled"), "{gopls:?}");
        Ok(())
    }

    #[test]
    fn missing_executable_warns_with_install_advice() -> TestResult {
        let (directory, effective) = effective(None)?;
        let findings = analyze_providers(directory.path(), &effective, false, Some(&[]));
        let gopls = findings
            .iter()
            .find(|finding| finding.subject == "provider:gopls")
            .ok_or("gopls finding")?;
        assert_eq!(gopls.severity, Severity::Warning, "{gopls:?}");
        assert!(gopls.advice.contains("docs/languages/go.md"), "{gopls:?}");
        Ok(())
    }

    #[test]
    fn misconfigured_path_is_an_error() -> TestResult {
        let (directory, _) = effective(Some("schema_version = 1\n"))?;
        let private = directory
            .path()
            .join(crate::config::PRIVATE_CONFIG_FILENAME);
        std::fs::write(
            &private,
            "schema_version = 1\n\n[providers.clangd]\npath = \"/nonexistent/clangd\"\n",
        )?;
        let effective = ConfigLayers::load(directory.path(), None)?.merge();
        let findings = analyze_providers(directory.path(), &effective, false, Some(&[]));
        let clangd = findings
            .iter()
            .find(|finding| finding.subject == "provider:clangd")
            .ok_or("clangd finding")?;
        assert_eq!(clangd.severity, Severity::Error, "{clangd:?}");
        Ok(())
    }

    #[test]
    fn missing_project_metadata_warns() -> TestResult {
        let (directory, effective) = effective(None)?;
        let bin = tempfile::tempdir()?;
        let fake = bin.path().join("gopls");
        std::fs::write(&fake, "#!/bin/sh\necho 'gopls v0.23.1'\n")?;
        make_executable(&fake)?;
        let entries = [bin.path().to_path_buf()];
        let findings = analyze_providers(directory.path(), &effective, false, Some(&entries));
        let gopls = findings
            .iter()
            .find(|finding| finding.subject == "provider:gopls")
            .ok_or("gopls finding")?;
        assert_eq!(gopls.severity, Severity::Warning, "{gopls:?}");
        assert!(gopls.evidence.contains("project metadata"), "{gopls:?}");
        Ok(())
    }

    #[cfg(unix)]
    fn make_executable(path: &Path) -> TestResult {
        use std::os::unix::fs::PermissionsExt as _;
        let mut permissions = std::fs::metadata(path)?.permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(path, permissions)?;
        Ok(())
    }

    #[cfg(not(unix))]
    fn make_executable(_path: &Path) -> TestResult {
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn probe_parses_version_and_flags_incompatible() -> TestResult {
        let (directory, effective) = effective(None)?;
        std::fs::write(directory.path().join("go.mod"), "module example\n")?;
        let bin = tempfile::tempdir()?;
        let fake = bin.path().join("gopls");
        std::fs::write(
            &fake,
            "#!/bin/sh\necho 'golang.org/x/tools/gopls v0.99.0'\n",
        )?;
        make_executable(&fake)?;
        let entries = [bin.path().to_path_buf()];
        let findings = analyze_providers(directory.path(), &effective, true, Some(&entries));
        let gopls = findings
            .iter()
            .find(|finding| finding.subject == "provider:gopls")
            .ok_or("gopls finding")?;
        assert_eq!(gopls.severity, Severity::Warning, "{gopls:?}");
        assert!(gopls.evidence.contains("0.99.0"), "{gopls:?}");
        assert!(gopls.evidence.contains("expected"), "{gopls:?}");
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn probe_timeout_is_bounded_and_reaped() -> TestResult {
        let bin = tempfile::tempdir()?;
        let fake = bin.path().join("sleepy");
        std::fs::write(&fake, "#!/bin/sh\nsleep 30\n")?;
        make_executable(&fake)?;
        let started = Instant::now();
        let outcome = probe_version(&fake, &[]);
        assert_eq!(outcome, ProbeOutcome::TimedOut);
        assert!(
            started.elapsed() < PROBE_DEADLINE + Duration::from_secs(2),
            "probe respected its deadline"
        );
        Ok(())
    }

    #[test]
    fn budget_finding_triggers_only_above_limit() -> TestResult {
        let directory = tempfile::tempdir()?;
        let git = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .current_dir(directory.path())
                .args(args)
                .output();
            let ok = matches!(&output, Ok(output) if output.status.success());
            assert!(ok, "git {args:?} failed: {output:?}");
        };
        git(&["init", "--quiet"]);
        for name in ["a.rs", "b.rs", "c.rs"] {
            std::fs::write(directory.path().join(name), "fn main() {}\n")?;
        }
        git(&["add", "."]);
        let shared = directory.path().join(crate::config::SHARED_CONFIG_FILENAME);
        std::fs::write(&shared, "schema_version = 1\n\n[index]\nmax_files = 2\n")?;
        let effective = ConfigLayers::load(directory.path(), None)?.merge();
        let finding = analyze_index_budget(directory.path(), &effective).ok_or("budget finding")?;
        assert_eq!(finding.severity, Severity::Warning, "{finding:?}");
        assert!(
            finding.evidence.contains("exceed index.max_files = 2"),
            "{finding:?}"
        );

        let directory = tempfile::tempdir()?;
        let effective = ConfigLayers::load(directory.path(), None)?.merge();
        std::process::Command::new("git")
            .current_dir(directory.path())
            .args(["init", "--quiet"])
            .status()?;
        std::fs::write(directory.path().join("a.rs"), "fn main() {}\n")?;
        std::process::Command::new("git")
            .current_dir(directory.path())
            .args(["add", "."])
            .status()?;
        let finding = analyze_index_budget(directory.path(), &effective).ok_or("budget finding")?;
        assert_eq!(finding.severity, Severity::Info, "{finding:?}");
        Ok(())
    }
}
