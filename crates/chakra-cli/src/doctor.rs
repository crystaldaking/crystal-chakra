//! `chakra doctor` connection and registration diagnostics (issue #205,
//! extended by #207/#208 under the same command; ADR-0055).
//!
//! Findings pair observed evidence with a scoped next step and carry a
//! deterministic code so reports and tests can match them. Exit status
//! distinguishes broken setup (1) from healthy or intentionally absent
//! configuration (0).

use std::fmt;
use std::path::{Path, PathBuf};

use crate::setup::{AgentClient, BLOCK_BEGIN, registration_command_for};

/// Finding severity; `Error` drives the non-zero exit status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Info,
    Warning,
    Error,
}

impl fmt::Display for Severity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Severity::Info => f.write_str("info"),
            Severity::Warning => f.write_str("warning"),
            Severity::Error => f.write_str("error"),
        }
    }
}

/// One diagnostic observation with evidence and a scoped next step.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Finding {
    pub code: &'static str,
    pub severity: Severity,
    pub subject: String,
    /// What the finding applies to (client, provider language, project).
    pub applicability: String,
    pub evidence: String,
    pub advice: String,
    /// Observed machine paths retained only for portable-report redaction.
    /// Do not rediscover them at export time: the executable may have moved.
    #[serde(skip)]
    pub(crate) private_paths: Vec<PathBuf>,
}

impl Finding {
    pub fn new(
        code: &'static str,
        severity: Severity,
        subject: String,
        evidence: String,
        advice: String,
    ) -> Self {
        Self {
            code,
            severity,
            subject,
            applicability: String::new(),
            evidence,
            advice,
            private_paths: Vec::new(),
        }
    }

    pub fn with_applicability(mut self, applicability: impl Into<String>) -> Self {
        self.applicability = applicability.into();
        self
    }

    pub(crate) fn with_private_path(mut self, path: &Path) -> Self {
        self.private_paths.push(path.to_owned());
        self
    }

    pub(crate) fn info(code: &'static str, subject: String, evidence: String) -> Self {
        Self::new(code, Severity::Info, subject, evidence, String::new())
    }

    pub(crate) fn warning(
        code: &'static str,
        subject: String,
        evidence: String,
        advice: String,
    ) -> Self {
        Self::new(code, Severity::Warning, subject, evidence, advice)
    }

    pub(crate) fn error(
        code: &'static str,
        subject: String,
        evidence: String,
        advice: String,
    ) -> Self {
        Self::new(code, Severity::Error, subject, evidence, advice)
    }
}

/// Locate a client executable on `PATH` (observation only; never spawned).
pub fn find_on_path(executable: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for directory in std::env::split_paths(&path_var) {
        let candidate = directory.join(executable);
        if is_executable_file(&candidate) {
            return Some(candidate);
        }
        #[cfg(windows)]
        for extension in ["exe", "cmd", "ps1", "bat"] {
            let candidate = directory.join(format!("{executable}.{extension}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

#[cfg(unix)]
pub fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    path.is_file()
        && path
            .metadata()
            .map(|metadata| metadata.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
}

#[cfg(not(unix))]
pub fn is_executable_file(path: &Path) -> bool {
    path.is_file()
}

/// Run the registration diagnostics (#205) plus provider/project analysis
/// (#207) for the selected clients against the resolved worktree root. When
/// `effective` is `None` (invalid configuration), analysis is skipped and
/// the configuration error is the finding. `probe` enables bounded
/// `--version` executions (isolated probe mode).
pub fn diagnose(
    root: &Path,
    exe: &Path,
    clients: &[AgentClient],
    effective: Option<&crate::config::EffectiveConfig>,
    probe: bool,
) -> Vec<Finding> {
    let mut findings = Vec::new();
    findings.push(crate::analysis::isolation_note());
    for client in clients {
        let name = client.as_str();
        match find_on_path(client.executable_name()) {
            Some(path) => findings.push(Finding::info(
                "client-executable",
                name.to_owned(),
                format!("{} found at {}", client.executable_name(), path.display()),
            ).with_private_path(&path)),
            None => findings.push(Finding::warning(
                "client-executable",
                name.to_owned(),
                format!("{} was not found on PATH", client.executable_name()),
                format!(
                    "install {name} or skip it; setup can still be written with chakra init --agent {name}"
                ),
            )),
        }

        let config_path = root.join(client.mcp_config_relative());
        match std::fs::read_to_string(&config_path) {
            Err(_) => findings.push(Finding::info(
                "mcp-registration",
                name.to_owned(),
                format!("{} not present", config_path.display()),
            )),
            Ok(text) => {
                findings.push(check_registration(client, &config_path, &text, exe, root));
            }
        }

        let instruction_path = root.join(client.instruction_relative());
        match std::fs::read_to_string(&instruction_path) {
            Ok(text) if text.contains(BLOCK_BEGIN) => findings.push(Finding::info(
                "instructions",
                name.to_owned(),
                format!(
                    "Chakra instruction block present in {}",
                    instruction_path.display()
                ),
            )),
            _ => findings.push(Finding::warning(
                "instructions",
                name.to_owned(),
                format!(
                    "no Chakra instruction block in {}",
                    instruction_path.display()
                ),
                format!("run chakra init --agent {name} to install the managed block"),
            )),
        }
    }

    let shared = root.join(crate::config::SHARED_CONFIG_FILENAME);
    match effective {
        Some(effective) => {
            findings.push(Finding::info(
                "project-config",
                "project".to_owned(),
                if shared.exists() {
                    format!("{} loads cleanly", shared.display())
                } else {
                    "no chakra.toml; built-in defaults apply".to_owned()
                },
            ));
            findings.extend(crate::analysis::analyze_providers(
                root, effective, probe, None,
            ));
            if let Some(finding) = crate::analysis::analyze_index_budget(root, effective) {
                findings.push(finding);
            }
        }
        None => {
            findings.push(Finding::warning(
                "project-config",
                "project".to_owned(),
                "configuration could not be loaded (see the error above); provider and budget analysis skipped".to_owned(),
                "fix the configuration, then re-run doctor".to_owned(),
            ));
        }
    }
    findings
}

fn check_registration(
    client: &AgentClient,
    path: &Path,
    text: &str,
    exe: &Path,
    root: &Path,
) -> Finding {
    let name = client.as_str();
    let entry = match client {
        AgentClient::Codex => text
            .parse::<toml_edit::DocumentMut>()
            .ok()
            .and_then(|document| {
                let entry = document.get("mcp_servers")?.get("chakra")?;
                let command = entry
                    .get("command")?
                    .as_str()
                    .unwrap_or_default()
                    .to_owned();
                let args: Vec<String> = entry
                    .get("args")
                    .and_then(toml_edit::Item::as_array)
                    .map(|array| {
                        array
                            .iter()
                            .filter_map(|value| value.as_str().map(str::to_owned))
                            .collect()
                    })
                    .unwrap_or_default();
                if command.is_empty() {
                    None
                } else {
                    Some((command, args))
                }
            }),
        AgentClient::Claude | AgentClient::Cursor => {
            serde_json::from_str::<serde_json::Value>(text)
                .ok()
                .and_then(|document| {
                    let entry = &document["mcpServers"]["chakra"];
                    let command = entry["command"].as_str().unwrap_or_default().to_owned();
                    let args: Vec<String> = entry["args"]
                        .as_array()
                        .map(|array| {
                            array
                                .iter()
                                .filter_map(|value| value.as_str().map(str::to_owned))
                                .collect()
                        })
                        .unwrap_or_default();
                    if command.is_empty() {
                        None
                    } else {
                        Some((command, args))
                    }
                })
        }
        AgentClient::Opencode => serde_json::from_str::<serde_json::Value>(text)
            .ok()
            .and_then(|document| {
                let command: Vec<String> = document["mcp"]["chakra"]["command"]
                    .as_array()
                    .map(|array| {
                        array
                            .iter()
                            .filter_map(|value| value.as_str().map(str::to_owned))
                            .collect()
                    })
                    .unwrap_or_default();
                let (first, rest) = command.split_first()?;
                Some((first.clone(), rest.to_vec()))
            }),
    };
    match entry {
        None => Finding::warning(
            "mcp-registration",
            name.to_owned(),
            format!("no chakra MCP entry in {}", path.display()),
            format!("run chakra init --agent {name}"),
        ),
        Some((command, args)) => {
            let (want_command, want_args) = registration_command_for(exe, root);
            if command == want_command && args == want_args {
                Finding::info(
                    "mcp-registration",
                    name.to_owned(),
                    format!(
                        "chakra MCP entry in {} matches this install and worktree",
                        path.display()
                    ),
                )
            } else {
                Finding::error(
                    "mcp-registration",
                    name.to_owned(),
                    format!(
                        "chakra MCP entry in {} points at `{command} {}` but this install/worktree expects `{want_command} {}`",
                        path.display(),
                        args.join(" "),
                        want_args.join(" ")
                    ),
                    format!("re-run chakra init --agent {name} after reviewing the existing entry"),
                )
            }
        }
    }
}

/// Versioned JSON representation of a doctor run (issue #207; consumed by
/// the #208 report export). The scope field marks the output as an isolated
/// inspection, never the agent's live session.
pub fn report_json(findings: &[Finding], worktree: &Path) -> Result<String, serde_json::Error> {
    #[derive(serde::Serialize)]
    struct DoctorDocument<'a> {
        schema_version: u32,
        kind: &'static str,
        scope: &'static str,
        chakra_version: &'static str,
        worktree: String,
        findings: &'a [Finding],
    }
    let document = DoctorDocument {
        schema_version: 1,
        kind: "chakra-doctor",
        scope: "isolated-inspection",
        chakra_version: env!("CARGO_PKG_VERSION"),
        worktree: worktree.to_string_lossy().into_owned(),
        findings,
    };
    serde_json::to_string_pretty(&document)
}

/// Print findings and derive the exit status (1 when any error exists).
pub fn report(findings: &[Finding]) -> u8 {
    let mut failed = false;
    for finding in findings {
        failed |= finding.severity == Severity::Error;
        println!(
            "[{}] {} ({}): {}",
            finding.severity, finding.code, finding.subject, finding.evidence
        );
        if !finding.advice.is_empty() {
            println!("    next: {}", finding.advice);
        }
    }
    u8::from(failed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::setup::{apply_plan, plan_setup};

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn exe() -> PathBuf {
        PathBuf::from("/usr/local/bin/chakra")
    }

    fn effective(
        directory: &Path,
    ) -> Result<crate::config::EffectiveConfig, Box<dyn std::error::Error>> {
        Ok(crate::config::ConfigLayers::load(directory, None)?.merge())
    }

    #[test]
    fn codex_without_chakra_registration_is_a_warning_not_a_panic() {
        for text in [
            "model = \"example\"\n",
            "[mcp_servers.other]\ncommand = \"other\"\n",
            "[mcp_servers.chakra]\nenabled = false\n",
            "mcp_servers = false\n",
        ] {
            let finding = check_registration(
                &AgentClient::Codex,
                Path::new(".codex/config.toml"),
                text,
                &exe(),
                Path::new("/project"),
            );
            assert_eq!(finding.severity, Severity::Warning, "{text}");
            assert_eq!(finding.code, "mcp-registration");
        }
    }

    #[test]
    fn fresh_project_reports_unconfigured_without_errors() -> TestResult {
        let directory = tempfile::tempdir()?;
        let findings = diagnose(
            directory.path(),
            &exe(),
            &[AgentClient::Claude],
            Some(&effective(directory.path())?),
            false,
        );
        assert!(findings.iter().any(
            |finding| finding.code == "mcp-registration" && finding.severity == Severity::Info
        ));
        assert!(
            findings
                .iter()
                .any(|finding| finding.code == "instructions"
                    && finding.severity == Severity::Warning)
        );
        assert_eq!(report(&findings), 0, "unconfigured is not an error");
        Ok(())
    }

    #[test]
    fn configured_project_reports_matching_registration() -> TestResult {
        let directory = tempfile::tempdir()?;
        let plan = plan_setup(directory.path(), &exe(), &[AgentClient::Claude])?;
        apply_plan(&plan)?;
        let findings = diagnose(
            directory.path(),
            &exe(),
            &[AgentClient::Claude],
            Some(&effective(directory.path())?),
            false,
        );
        let registration = findings
            .iter()
            .find(|finding| finding.code == "mcp-registration")
            .ok_or("registration finding")?;
        assert_eq!(registration.severity, Severity::Info, "{registration:?}");
        let instructions = findings
            .iter()
            .find(|finding| finding.code == "instructions")
            .ok_or("instruction finding")?;
        assert_eq!(instructions.severity, Severity::Info, "{instructions:?}");
        assert_eq!(report(&findings), 0);
        Ok(())
    }

    #[test]
    fn stale_registration_is_an_error_with_exit_status() -> TestResult {
        let directory = tempfile::tempdir()?;
        std::fs::write(
            directory.path().join(".mcp.json"),
            "{\"mcpServers\":{\"chakra\":{\"command\":\"/moved/chakra\",\"args\":[\"serve\",\"--repo\",\"/elsewhere\"]}}}",
        )?;
        let findings = diagnose(
            directory.path(),
            &exe(),
            &[AgentClient::Claude],
            Some(&effective(directory.path())?),
            false,
        );
        let registration = findings
            .iter()
            .find(|finding| finding.code == "mcp-registration")
            .ok_or("registration finding")?;
        assert_eq!(registration.severity, Severity::Error, "{registration:?}");
        assert!(
            registration.evidence.contains("/moved/chakra"),
            "{registration:?}"
        );
        assert_eq!(report(&findings), 1);
        Ok(())
    }

    #[test]
    fn invalid_project_config_is_an_error() -> TestResult {
        let directory = tempfile::tempdir()?;
        std::fs::write(
            directory.path().join(crate::config::SHARED_CONFIG_FILENAME),
            "schema_version = 99\n",
        )?;
        let findings = diagnose(
            directory.path(),
            &exe(),
            &[AgentClient::Claude],
            None,
            false,
        );
        assert!(
            findings
                .iter()
                .any(|finding| finding.code == "project-config"
                    && finding.severity == Severity::Warning),
            "{findings:?}"
        );
        assert_eq!(report(&findings), 0);
        Ok(())
    }

    #[test]
    fn json_document_is_versioned_and_marked_isolated() -> TestResult {
        let directory = tempfile::tempdir()?;
        let findings = diagnose(
            directory.path(),
            &exe(),
            &[AgentClient::Claude],
            Some(&effective(directory.path())?),
            false,
        );
        let json = report_json(&findings, directory.path())?;
        let parsed: serde_json::Value = serde_json::from_str(&json)?;
        assert_eq!(parsed["schema_version"], 1);
        assert_eq!(parsed["scope"], "isolated-inspection");
        assert!(parsed["findings"].is_array());
        Ok(())
    }
}
