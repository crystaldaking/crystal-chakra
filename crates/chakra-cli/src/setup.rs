//! One-time agent-client project setup (issue #205, ADR-0055).
//!
//! `chakra init` registers the Chakra MCP server in a supported client's
//! project-scope configuration and installs a delimited instruction block.
//! Chakra owns exactly the `chakra` server entry, the marked instruction
//! block, and (when absent) a minimal `chakra.toml`; everything else is
//! preserved. All writes are planned first so `--dry-run` is pure, and every
//! apply goes through a temporary file plus atomic rename.

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

/// Supported agent clients (pinned formats in ADR-0055).
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum AgentClient {
    Codex,
    Claude,
    Cursor,
    Opencode,
}

impl AgentClient {
    pub const ALL: &'static [AgentClient] = &[
        AgentClient::Codex,
        AgentClient::Claude,
        AgentClient::Cursor,
        AgentClient::Opencode,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            AgentClient::Codex => "codex",
            AgentClient::Claude => "claude",
            AgentClient::Cursor => "cursor",
            AgentClient::Opencode => "opencode",
        }
    }

    /// The client executable looked up on `PATH` for diagnostics.
    pub fn executable_name(self) -> &'static str {
        self.as_str()
    }

    /// Project-scope MCP configuration file, relative to the worktree root.
    pub fn mcp_config_relative(self) -> PathBuf {
        match self {
            AgentClient::Codex => PathBuf::from(".codex").join("config.toml"),
            AgentClient::Claude => PathBuf::from(".mcp.json"),
            AgentClient::Cursor => PathBuf::from(".cursor").join("mcp.json"),
            AgentClient::Opencode => PathBuf::from("opencode.json"),
        }
    }

    /// Project instruction file carrying the Chakra-managed block.
    pub fn instruction_relative(self) -> PathBuf {
        match self {
            AgentClient::Claude => PathBuf::from("CLAUDE.md"),
            _ => PathBuf::from("AGENTS.md"),
        }
    }
}

/// Setup or diagnostic failure with an actionable message (ADR-0055).
#[derive(Debug)]
pub struct SetupError(pub String);

impl fmt::Display for SetupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for SetupError {}

fn error(message: impl Into<String>) -> SetupError {
    SetupError(message.into())
}

/// What happens to one file when the plan is applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteAction {
    Create,
    Update,
    Delete,
}

impl fmt::Display for WriteAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WriteAction::Create => f.write_str("create"),
            WriteAction::Update => f.write_str("update"),
            WriteAction::Delete => f.write_str("delete"),
        }
    }
}

/// One planned filesystem change.
#[derive(Debug, Clone)]
pub struct PlannedWrite {
    pub path: PathBuf,
    pub action: WriteAction,
    /// New full file content for Create/Update; `None` for Delete.
    pub content: Option<String>,
    pub summary: String,
}

/// The complete setup or removal plan for one worktree.
#[derive(Debug, Default)]
pub struct SetupPlan {
    pub writes: Vec<PlannedWrite>,
    pub notes: Vec<String>,
}

/// Marker opening the Chakra-managed instruction block.
pub const BLOCK_BEGIN: &str = "<!-- chakra:begin -->";
/// Marker closing the Chakra-managed instruction block.
pub const BLOCK_END: &str = "<!-- chakra:end -->";

/// The shared concise workflow template rendered into every client's
/// instruction file (ADR-0055).
pub const INSTRUCTION_TEMPLATE: &str = r#"## Chakra code intelligence (managed by `chakra init`)

This repository is served by Chakra, a local code-intelligence MCP server.

- Start sessions with the Chakra `status` tool to confirm the served
  worktree, revision, freshness, and provider state match this checkout.
- Prefer Chakra tools for code discovery (`repo_map`, `search`,
  `symbol_search`), entity detail (`context`, `callers`), and change
  analysis (`diff_context`) over ad-hoc file scanning.
- Respect the `precision`/`provenance` labels in Chakra results: treat
  `syntax` and heuristic candidates as leads to verify, not precise facts.
- When several worktrees are registered, pass the intended `workspace_id`.
- If Chakra is unavailable or does not cover the request, use ordinary
  search and say that Chakra was not used.
"#;

/// Minimal shared configuration installed when the project has none
/// (ADR-0053); an existing `chakra.toml` is never overwritten.
pub const CHAKRA_TOML_TEMPLATE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../docs/examples/chakra.toml"
));

fn read_optional(path: &Path) -> Result<Option<String>, SetupError> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(io_error) if io_error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(io_error) => Err(error(format!("cannot read {}: {io_error}", path.display()))),
    }
}

/// Insert or refresh the managed block, preserving everything outside the
/// markers byte-for-byte. Malformed marker pairs are an error (ADR-0055).
pub fn upsert_block(existing: Option<&str>) -> Result<String, SetupError> {
    let block = format!("{BLOCK_BEGIN}\n{INSTRUCTION_TEMPLATE}{BLOCK_END}");
    let Some(existing) = existing else {
        return Ok(format!("{block}\n"));
    };
    if !existing.contains(BLOCK_BEGIN) && !existing.contains(BLOCK_END) {
        let separator = if existing.ends_with('\n') {
            "\n"
        } else {
            "\n\n"
        };
        return Ok(format!("{existing}{separator}{block}\n"));
    }
    let begin = existing.find(BLOCK_BEGIN);
    let end = existing.find(BLOCK_END);
    match (begin, end) {
        (Some(begin), Some(end))
            if begin < end
                && !existing[end + BLOCK_END.len()..].contains(BLOCK_END)
                && !existing[begin + BLOCK_BEGIN.len()..end].contains(BLOCK_BEGIN) =>
        {
            let after = &existing[end + BLOCK_END.len()..];
            Ok(format!("{}{block}{}", &existing[..begin], after))
        }
        _ => Err(error(
            "malformed Chakra instruction block (unbalanced or nested chakra markers); fix or remove the markers manually",
        )),
    }
}

/// Remove the managed block. Returns `None` when the file would be empty
/// and should be deleted.
pub fn strip_block(existing: &str) -> Result<Option<String>, SetupError> {
    if !existing.contains(BLOCK_BEGIN) && !existing.contains(BLOCK_END) {
        return Ok(Some(existing.to_owned()));
    }
    let begin = existing.find(BLOCK_BEGIN);
    let end = existing.find(BLOCK_END);
    match (begin, end) {
        (Some(begin), Some(end)) if begin < end => {
            let mut remaining = format!(
                "{}{}",
                &existing[..begin],
                &existing[end + BLOCK_END.len()..]
            );
            while remaining.contains("\n\n\n") {
                remaining = remaining.replace("\n\n\n", "\n\n");
            }
            if remaining.trim().is_empty() {
                Ok(None)
            } else {
                Ok(Some(remaining))
            }
        }
        _ => Err(error(
            "malformed Chakra instruction block; fix or remove the markers manually",
        )),
    }
}

/// The registered command line for this worktree.
pub fn registration_command_for(exe: &Path, root: &Path) -> (String, Vec<String>) {
    (
        exe.to_string_lossy().into_owned(),
        vec![
            "serve".to_owned(),
            "--repo".to_owned(),
            root.to_string_lossy().into_owned(),
        ],
    )
}

fn same_registration(command: &str, args: &[String], exe: &Path, root: &Path) -> bool {
    let (want_command, want_args) = registration_command_for(exe, root);
    command == want_command && args == want_args
}

fn conflict(path: &Path, client: &str) -> SetupError {
    error(format!(
        "{} already contains a different `chakra` MCP entry for {client}; remove it first (chakra init --agent {client} --remove) or edit {} manually",
        path.display(),
        path.display()
    ))
}

// --- Codex: .codex/config.toml (TOML, comment-preserving via toml_edit) ---

fn codex_upsert(
    existing: Option<&str>,
    path: &Path,
    exe: &Path,
    root: &Path,
) -> Result<String, SetupError> {
    let mut document = existing
        .unwrap_or("")
        .parse::<toml_edit::DocumentMut>()
        .map_err(|parse| error(format!("{} is not valid TOML: {parse}", path.display())))?;
    let (command, args) = registration_command_for(exe, root);
    let servers = document["mcp_servers"].or_insert(toml_edit::table());
    let server = servers["chakra"].or_insert(toml_edit::table());
    if let Some(existing_table) = server.as_table_like() {
        let existing_command = existing_table
            .get("command")
            .and_then(toml_edit::Item::as_str)
            .unwrap_or_default()
            .to_owned();
        let existing_args: Vec<String> = existing_table
            .get("args")
            .and_then(toml_edit::Item::as_array)
            .map(|array| {
                array
                    .iter()
                    .filter_map(|value| value.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        if !existing_command.is_empty()
            && !same_registration(&existing_command, &existing_args, exe, root)
        {
            return Err(conflict(path, "codex"));
        }
    }
    let server = servers["chakra"].or_insert(toml_edit::table());
    let table = server
        .as_table_like_mut()
        .ok_or_else(|| error("mcp_servers.chakra is not a table".to_owned()))?;
    table.insert("command", toml_edit::value(command));
    let mut array = toml_edit::Array::new();
    for arg in args {
        array.push(arg);
    }
    table.insert("args", toml_edit::value(array));
    table.insert("enabled", toml_edit::value(true));
    Ok(document.to_string())
}

fn codex_remove(existing: &str, path: &Path) -> Result<Option<String>, SetupError> {
    let mut document = existing
        .parse::<toml_edit::DocumentMut>()
        .map_err(|parse| error(format!("{} is not valid TOML: {parse}", path.display())))?;
    if let Some(servers) = document
        .get_mut("mcp_servers")
        .and_then(toml_edit::Item::as_table_like_mut)
    {
        servers.remove("chakra");
        if servers.iter().count() == 0
            && let Some(item) = document.get_mut("mcp_servers")
        {
            *item = toml_edit::Item::None;
        }
    }
    let text = document.to_string();
    if text.trim().is_empty() {
        Ok(None)
    } else {
        Ok(Some(text))
    }
}

// --- JSON adapters: Claude .mcp.json, Cursor .cursor/mcp.json ---

fn parse_json(existing: Option<&str>, path: &Path) -> Result<serde_json::Value, SetupError> {
    match existing {
        None => Ok(serde_json::json!({})),
        Some(text) if text.trim().is_empty() => Ok(serde_json::json!({})),
        Some(text) => serde_json::from_str(text).map_err(|parse| {
            error(format!(
                "{} is not valid JSON: {parse}; fix it manually (comments/JSONC are not rewritten)",
                path.display()
            ))
        }),
    }
}

fn json_upsert(
    existing: Option<&str>,
    path: &Path,
    client: &str,
    exe: &Path,
    root: &Path,
) -> Result<String, SetupError> {
    let mut document = parse_json(existing, path)?;
    let (command, args) = registration_command_for(exe, root);
    let servers = document
        .as_object_mut()
        .ok_or_else(|| error(format!("{} must contain a JSON object", path.display())))?
        .entry("mcpServers")
        .or_insert_with(|| serde_json::json!({}));
    let servers = servers
        .as_object_mut()
        .ok_or_else(|| error(format!("{}.mcpServers must be an object", path.display())))?;
    if let Some(existing_entry) = servers.get("chakra") {
        let existing_command = existing_entry["command"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        let existing_args: Vec<String> = existing_entry["args"]
            .as_array()
            .map(|array| {
                array
                    .iter()
                    .filter_map(|value| value.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        if !existing_command.is_empty()
            && !same_registration(&existing_command, &existing_args, exe, root)
        {
            return Err(conflict(path, client));
        }
    }
    servers.insert(
        "chakra".to_owned(),
        serde_json::json!({ "command": command, "args": args }),
    );
    let mut text = serde_json::to_string_pretty(&document)
        .map_err(|serialize| error(format!("cannot serialize {}: {serialize}", path.display())))?;
    text.push('\n');
    Ok(text)
}

fn json_remove(existing: &str, path: &Path) -> Result<Option<String>, SetupError> {
    let mut document = parse_json(Some(existing), path)?;
    if let Some(servers) = document
        .as_object_mut()
        .and_then(|object| object.get_mut("mcpServers"))
        .and_then(serde_json::Value::as_object_mut)
    {
        servers.remove("chakra");
    }
    let empty = document
        .as_object()
        .map(|object| {
            object.is_empty()
                || object
                    .get("mcpServers")
                    .and_then(serde_json::Value::as_object)
                    .map(serde_json::Map::is_empty)
                    .unwrap_or(false)
                    && object.len() == 1
        })
        .unwrap_or(false);
    if empty {
        return Ok(None);
    }
    let mut text = serde_json::to_string_pretty(&document)
        .map_err(|serialize| error(format!("cannot serialize {}: {serialize}", path.display())))?;
    text.push('\n');
    Ok(Some(text))
}

// --- OpenCode: opencode.json with the V2 `mcp` shape ---

fn opencode_upsert(
    existing: Option<&str>,
    path: &Path,
    exe: &Path,
    root: &Path,
) -> Result<String, SetupError> {
    let mut document = parse_json(existing, path)?;
    if document.get("mcpServers").is_some() {
        return Err(error(format!(
            "{} uses the legacy `mcpServers` layout; supported OpenCode configuration uses the top-level `mcp` object — migrate it manually (see docs/support/agent-clients.md)",
            path.display()
        )));
    }
    let (command, args) = registration_command_for(exe, root);
    let mut command_line = vec![command];
    command_line.extend(args);
    let servers = document
        .as_object_mut()
        .ok_or_else(|| error(format!("{} must contain a JSON object", path.display())))?
        .entry("mcp")
        .or_insert_with(|| serde_json::json!({}));
    let servers = servers
        .as_object_mut()
        .ok_or_else(|| error(format!("{}.mcp must be an object", path.display())))?;
    if let Some(existing_entry) = servers.get("chakra") {
        let existing_command: Vec<String> = existing_entry["command"]
            .as_array()
            .map(|array| {
                array
                    .iter()
                    .filter_map(|value| value.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        let (want_command, want_args) = registration_command_for(exe, root);
        let mut want = vec![want_command];
        want.extend(want_args);
        if !existing_command.is_empty() && existing_command != want {
            return Err(conflict(path, "opencode"));
        }
    }
    servers.insert(
        "chakra".to_owned(),
        serde_json::json!({
            "type": "local",
            "command": command_line,
            "enabled": true,
        }),
    );
    let mut text = serde_json::to_string_pretty(&document)
        .map_err(|serialize| error(format!("cannot serialize {}: {serialize}", path.display())))?;
    text.push('\n');
    Ok(text)
}

fn opencode_remove(existing: &str, path: &Path) -> Result<Option<String>, SetupError> {
    let mut document = parse_json(Some(existing), path)?;
    if let Some(servers) = document
        .as_object_mut()
        .and_then(|object| object.get_mut("mcp"))
        .and_then(serde_json::Value::as_object_mut)
    {
        servers.remove("chakra");
    }
    let empty = document
        .as_object()
        .map(|object| {
            object.is_empty()
                || object
                    .get("mcp")
                    .and_then(serde_json::Value::as_object)
                    .map(serde_json::Map::is_empty)
                    .unwrap_or(false)
                    && object.len() == 1
        })
        .unwrap_or(false);
    if empty {
        return Ok(None);
    }
    let mut text = serde_json::to_string_pretty(&document)
        .map_err(|serialize| error(format!("cannot serialize {}: {serialize}", path.display())))?;
    text.push('\n');
    Ok(Some(text))
}

/// Merge duplicate writes to the same path (several clients share one
/// `AGENTS.md`); identical plans computed from the same pre-state collapse
/// to the first entry so apply never deletes or writes a file twice.
fn dedupe_writes(writes: Vec<PlannedWrite>) -> Vec<PlannedWrite> {
    let mut seen = std::collections::BTreeSet::new();
    writes
        .into_iter()
        .filter(|write| seen.insert(write.path.clone()))
        .collect()
}

/// Plan setup for the selected clients in the worktree at `root` (already
/// resolved through `chakra-git`). Pure: reads but never writes.
pub fn plan_setup(
    root: &Path,
    exe: &Path,
    clients: &[AgentClient],
) -> Result<SetupPlan, SetupError> {
    let mut plan = SetupPlan::default();
    for client in clients {
        let (config_path, content) = {
            let path = root.join(client.mcp_config_relative());
            let existing = read_optional(&path)?;
            let new = match client {
                AgentClient::Codex => codex_upsert(existing.as_deref(), &path, exe, root)?,
                AgentClient::Claude => {
                    json_upsert(existing.as_deref(), &path, "claude", exe, root)?
                }
                AgentClient::Cursor => {
                    json_upsert(existing.as_deref(), &path, "cursor", exe, root)?
                }
                AgentClient::Opencode => opencode_upsert(existing.as_deref(), &path, exe, root)?,
            };
            (path, new)
        };
        if existing_differs(&config_path, &content)? {
            plan.writes.push(PlannedWrite {
                action: if config_path.exists() {
                    WriteAction::Update
                } else {
                    WriteAction::Create
                },
                path: config_path,
                content: Some(content),
                summary: format!("register the chakra MCP server for {}", client.as_str()),
            });
        } else {
            plan.notes.push(format!(
                "{}: chakra MCP registration already up to date",
                client.as_str()
            ));
        }

        let instruction_path = root.join(client.instruction_relative());
        let existing = read_optional(&instruction_path)?;
        let content = upsert_block(existing.as_deref())?;
        if existing.as_deref() != Some(content.as_str()) {
            plan.writes.push(PlannedWrite {
                action: if instruction_path.exists() {
                    WriteAction::Update
                } else {
                    WriteAction::Create
                },
                path: instruction_path,
                content: Some(content),
                summary: format!(
                    "install the Chakra instruction block for {}",
                    client.as_str()
                ),
            });
        } else {
            plan.notes.push(format!(
                "{}: instruction block already up to date",
                client.as_str()
            ));
        }
    }

    let shared = root.join(crate::config::SHARED_CONFIG_FILENAME);
    if !shared.exists() && fs::symlink_metadata(&shared).is_err() {
        plan.writes.push(PlannedWrite {
            path: shared,
            action: WriteAction::Create,
            content: Some(CHAKRA_TOML_TEMPLATE.to_owned()),
            summary: "create a minimal shared chakra.toml".to_owned(),
        });
    } else {
        plan.notes
            .push("chakra.toml already exists; left untouched".to_owned());
    }
    plan.writes = dedupe_writes(plan.writes);
    Ok(plan)
}

fn existing_differs(path: &Path, content: &str) -> Result<bool, SetupError> {
    Ok(read_optional(path)?.as_deref() != Some(content))
}

/// Plan removal of Chakra-owned setup. Pure: reads but never writes.
pub fn plan_removal(root: &Path, clients: &[AgentClient]) -> Result<SetupPlan, SetupError> {
    let mut plan = SetupPlan::default();
    for client in clients {
        let config_path = root.join(client.mcp_config_relative());
        if let Some(existing) = read_optional(&config_path)? {
            let removed = match client {
                AgentClient::Codex => codex_remove(&existing, &config_path)?,
                AgentClient::Claude | AgentClient::Cursor => json_remove(&existing, &config_path)?,
                AgentClient::Opencode => opencode_remove(&existing, &config_path)?,
            };
            if removed.as_deref() != Some(existing.as_str()) {
                plan.writes.push(PlannedWrite {
                    action: match removed {
                        Some(_) => WriteAction::Update,
                        None => WriteAction::Delete,
                    },
                    path: config_path,
                    content: removed,
                    summary: format!("remove the chakra MCP entry for {}", client.as_str()),
                });
            }
        }
        let instruction_path = root.join(client.instruction_relative());
        if let Some(existing) = read_optional(&instruction_path)? {
            let stripped = strip_block(&existing)?;
            if stripped.as_deref() != Some(existing.as_str()) {
                plan.writes.push(PlannedWrite {
                    action: match stripped {
                        Some(_) => WriteAction::Update,
                        None => WriteAction::Delete,
                    },
                    path: instruction_path,
                    content: stripped,
                    summary: format!(
                        "remove the Chakra instruction block for {}",
                        client.as_str()
                    ),
                });
            }
        }
    }
    if plan.writes.is_empty() {
        plan.notes.push("nothing Chakra-owned to remove".to_owned());
    } else {
        plan.notes
            .push("chakra.toml is project configuration and is left in place".to_owned());
    }
    plan.writes = dedupe_writes(plan.writes);
    Ok(plan)
}

/// Apply a plan atomically per file (temporary file + rename).
pub fn apply_plan(plan: &SetupPlan) -> Result<(), SetupError> {
    for write in &plan.writes {
        match write.action {
            WriteAction::Create | WriteAction::Update => {
                let content = write.content.as_ref().ok_or_else(|| {
                    error(format!(
                        "{}: missing content for write",
                        write.path.display()
                    ))
                })?;
                if let Some(parent) = write.path.parent() {
                    fs::create_dir_all(parent)
                        .map_err(|io| error(format!("cannot create {}: {io}", parent.display())))?;
                }
                let temporary = write.path.with_extension("chakra-tmp");
                fs::write(&temporary, content)
                    .map_err(|io| error(format!("cannot write {}: {io}", write.path.display())))?;
                fs::rename(&temporary, &write.path).map_err(|io| {
                    let _ = fs::remove_file(&temporary);
                    error(format!("cannot install {}: {io}", write.path.display()))
                })?;
            }
            WriteAction::Delete => {
                fs::remove_file(&write.path)
                    .map_err(|io| error(format!("cannot remove {}: {io}", write.path.display())))?;
                // Prune now-empty adapter directories we may have created;
                // a non-empty directory is left alone.
                if let Some(parent) = write.path.parent() {
                    let _ = fs::remove_dir(parent);
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn root() -> Result<tempfile::TempDir, Box<dyn std::error::Error>> {
        Ok(tempfile::tempdir()?)
    }

    fn exe() -> PathBuf {
        PathBuf::from("/usr/local/bin/chakra")
    }

    #[test]
    fn codex_setup_writes_comment_preserving_registration() -> TestResult {
        let directory = root()?;
        let config = directory.path().join(".codex").join("config.toml");
        fs::create_dir_all(config.parent().ok_or("parent")?)?;
        fs::write(&config, "# user comment\nmodel = \"gpt-5\"\n")?;
        let plan = plan_setup(directory.path(), &exe(), &[AgentClient::Codex])?;
        assert!(plan.writes.iter().any(|write| write.path == config));
        apply_plan(&plan)?;
        let text = fs::read_to_string(&config)?;
        assert!(text.contains("# user comment"), "{text}");
        assert!(text.contains("model = \"gpt-5\""), "{text}");
        assert!(text.contains("[mcp_servers.chakra]"), "{text}");
        assert!(
            text.contains("command = \"/usr/local/bin/chakra\""),
            "{text}"
        );
        assert!(text.contains("enabled = true"), "{text}");
        Ok(())
    }

    #[test]
    fn codex_setup_is_idempotent_and_removes_cleanly() -> TestResult {
        let directory = root()?;
        let plan = plan_setup(directory.path(), &exe(), &[AgentClient::Codex])?;
        apply_plan(&plan)?;
        let second = plan_setup(directory.path(), &exe(), &[AgentClient::Codex])?;
        assert!(
            second.writes.iter().all(|write| write
                .path
                .file_name()
                .is_some_and(|name| name != "config.toml")),
            "second run must not rewrite the registration: {:?}",
            second
                .writes
                .iter()
                .map(|write| &write.path)
                .collect::<Vec<_>>()
        );
        let removal = plan_removal(directory.path(), &[AgentClient::Codex])?;
        apply_plan(&removal)?;
        let config = directory.path().join(".codex").join("config.toml");
        assert!(!config.exists(), "empty adapter config is deleted");
        Ok(())
    }

    #[test]
    fn codex_conflicting_registration_stops_setup() -> TestResult {
        let directory = root()?;
        let config = directory.path().join(".codex").join("config.toml");
        fs::create_dir_all(config.parent().ok_or("parent")?)?;
        fs::write(
            &config,
            "[mcp_servers.chakra]\ncommand = \"/old/chakra\"\nargs = [\"serve\"]\n",
        )?;
        let failure = plan_setup(directory.path(), &exe(), &[AgentClient::Codex])
            .err()
            .ok_or("conflicting entry must stop setup")?;
        assert!(
            failure.to_string().contains("different `chakra` MCP entry"),
            "{failure}"
        );
        let unchanged = fs::read_to_string(&config)?;
        assert!(unchanged.contains("/old/chakra"), "user entry preserved");
        Ok(())
    }

    #[test]
    fn claude_setup_preserves_other_servers_and_keys() -> TestResult {
        let directory = root()?;
        let config = directory.path().join(".mcp.json");
        fs::write(
            &config,
            "{\n  \"mcpServers\": {\n    \"other\": {\"command\": \"npx\", \"args\": [\"x\"]}\n  },\n  \"unrelated\": true\n}\n",
        )?;
        let plan = plan_setup(directory.path(), &exe(), &[AgentClient::Claude])?;
        apply_plan(&plan)?;
        let parsed: serde_json::Value = serde_json::from_str(&fs::read_to_string(&config)?)?;
        assert_eq!(parsed["mcpServers"]["other"]["command"], "npx");
        assert_eq!(parsed["unrelated"], true);
        assert_eq!(
            parsed["mcpServers"]["chakra"]["command"],
            "/usr/local/bin/chakra"
        );
        assert_eq!(
            parsed["mcpServers"]["chakra"]["args"][0],
            serde_json::Value::from("serve")
        );
        Ok(())
    }

    #[test]
    fn claude_conflicting_registration_stops_setup() -> TestResult {
        let directory = root()?;
        let config = directory.path().join(".mcp.json");
        fs::write(
            &config,
            "{\"mcpServers\":{\"chakra\":{\"command\":\"/old/chakra\",\"args\":[\"serve\"]}}}",
        )?;
        let failure = plan_setup(directory.path(), &exe(), &[AgentClient::Claude])
            .err()
            .ok_or("conflicting entry must stop setup")?;
        assert!(
            failure.to_string().contains("different `chakra` MCP entry"),
            "{failure}"
        );
        Ok(())
    }

    #[test]
    fn cursor_setup_writes_project_scope_registration() -> TestResult {
        let directory = root()?;
        let plan = plan_setup(directory.path(), &exe(), &[AgentClient::Cursor])?;
        apply_plan(&plan)?;
        let parsed: serde_json::Value = serde_json::from_str(&fs::read_to_string(
            directory.path().join(".cursor").join("mcp.json"),
        )?)?;
        assert_eq!(
            parsed["mcpServers"]["chakra"]["command"],
            "/usr/local/bin/chakra"
        );
        Ok(())
    }

    #[test]
    fn opencode_setup_uses_v2_shape_and_rejects_legacy() -> TestResult {
        let directory = root()?;
        let plan = plan_setup(directory.path(), &exe(), &[AgentClient::Opencode])?;
        apply_plan(&plan)?;
        let parsed: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(directory.path().join("opencode.json"))?)?;
        assert_eq!(parsed["mcp"]["chakra"]["type"], "local");
        assert_eq!(
            parsed["mcp"]["chakra"]["command"][1],
            serde_json::Value::from("serve")
        );
        assert_eq!(parsed["mcp"]["chakra"]["enabled"], true);

        let legacy = root()?;
        fs::write(
            legacy.path().join("opencode.json"),
            "{\"mcpServers\": {\"x\": {\"command\": \"npx\"}}}",
        )?;
        let failure = plan_setup(legacy.path(), &exe(), &[AgentClient::Opencode])
            .err()
            .ok_or("legacy shape must be rejected")?;
        assert!(
            failure.to_string().contains("legacy `mcpServers`"),
            "{failure}"
        );

        let commented = root()?;
        fs::write(
            commented.path().join("opencode.json"),
            "// comment\n{\"mcp\": {}}\n",
        )?;
        let failure = plan_setup(commented.path(), &exe(), &[AgentClient::Opencode])
            .err()
            .ok_or("JSONC comments must be refused")?;
        assert!(failure.to_string().contains("not valid JSON"), "{failure}");
        Ok(())
    }

    #[test]
    fn instruction_block_lifecycle_preserves_user_text() -> TestResult {
        // Create in an absent file.
        let created = upsert_block(None)?;
        assert!(created.contains(BLOCK_BEGIN));
        assert!(created.contains(BLOCK_END));
        // Append to an existing file, preserving user text.
        let appended = upsert_block(Some("# Team rules\n\nBe nice.\n"))?;
        assert!(
            appended.starts_with("# Team rules\n\nBe nice.\n"),
            "{appended}"
        );
        assert!(appended.contains(INSTRUCTION_TEMPLATE.trim()), "{appended}");
        // Refresh replaces only the managed content.
        let refreshed = upsert_block(Some(&appended.replace("served by Chakra", "STALE")))?;
        assert!(
            refreshed.starts_with("# Team rules\n\nBe nice.\n"),
            "{refreshed}"
        );
        assert!(refreshed.contains("served by Chakra"), "{refreshed}");
        // Malformed markers are an error, never a rewrite.
        assert!(upsert_block(Some(&format!("text {BLOCK_BEGIN} dangling"))).is_err());
        assert!(upsert_block(Some(&format!("text {BLOCK_END} dangling"))).is_err());
        // Strip returns user text; empty result means delete the file.
        let stripped = strip_block(&appended)?;
        assert_eq!(stripped.as_deref(), Some("# Team rules\n\nBe nice.\n\n"));
        let stripped = strip_block(&created)?;
        assert_eq!(stripped, None);
        Ok(())
    }

    #[test]
    fn setup_creates_chakra_toml_only_when_missing() -> TestResult {
        let directory = root()?;
        let plan = plan_setup(directory.path(), &exe(), &[AgentClient::Claude])?;
        assert!(
            plan.writes
                .iter()
                .any(|write| write.summary.contains("chakra.toml"))
        );
        apply_plan(&plan)?;
        let second = plan_setup(directory.path(), &exe(), &[AgentClient::Claude])?;
        assert!(
            second
                .writes
                .iter()
                .all(|write| !write.summary.contains("chakra.toml")),
            "existing chakra.toml is never overwritten"
        );
        Ok(())
    }

    #[test]
    fn full_cycle_setup_then_removal_leaves_no_chakra_files() -> TestResult {
        let directory = root()?;
        let clients = [
            AgentClient::Codex,
            AgentClient::Claude,
            AgentClient::Cursor,
            AgentClient::Opencode,
        ];
        let plan = plan_setup(directory.path(), &exe(), &clients)?;
        apply_plan(&plan)?;
        let removal = plan_removal(directory.path(), &clients)?;
        apply_plan(&removal)?;
        assert!(!directory.path().join(".mcp.json").exists());
        assert!(!directory.path().join("opencode.json").exists());
        // chakra.toml is project configuration and survives removal.
        assert!(directory.path().join("chakra.toml").exists());
        // AGENTS.md contained only the managed block and was pruned.
        assert!(!directory.path().join("AGENTS.md").exists());
        Ok(())
    }

    #[test]
    fn paths_with_spaces_and_non_ascii_work() -> TestResult {
        let parent = tempfile::tempdir()?;
        let spaced = parent.path().join("мой проект x");
        fs::create_dir_all(&spaced)?;
        let plan = plan_setup(&spaced, &exe(), &[AgentClient::Claude])?;
        apply_plan(&plan)?;
        let parsed: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(spaced.join(".mcp.json"))?)?;
        assert_eq!(
            parsed["mcpServers"]["chakra"]["args"][2],
            serde_json::Value::from(spaced.to_string_lossy().into_owned())
        );
        Ok(())
    }
}
