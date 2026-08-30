//! Hermetic terraform-ls lifecycle regressions using a scripted stdio LSP peer.

use std::error::Error;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use chakra_domain::location::{RepoRelativePath, SourceRange, TextPosition};
use chakra_domain::provenance::Provenance;
use chakra_domain::revision::Revision;
use chakra_domain::state::ProviderState;
use chakra_domain::symbol::Language;
use chakra_engine::{
    CallHierarchyDirections, PreciseProvider, PreciseQueryRequest, ProviderDocument,
    ProviderSymbol, ProviderWorkspace,
};
use chakra_provider_terraform_ls::{TerraformLsCommand, TerraformLsConfig, TerraformLsProvider};

const FAKE_SERVER: &str = r#"
use std::fs;
use std::io::{self, BufRead, Read, Write};
use std::process::Child;

fn request_id(body: &str) -> Option<&str> {
    let rest = body.split_once("\"id\":")?.1;
    let end = rest.find(|character: char| !character.is_ascii_digit())?;
    rest.get(..end)
}

fn request_uri(body: &str) -> Option<&str> {
    let rest = body.split_once("\"uri\":\"")?.1;
    rest.split_once('"').map(|(uri, _)| uri)
}

fn send(id: &str, result: &str) -> io::Result<()> {
    let body = format!("{{\"jsonrpc\":\"2.0\",\"id\":{id},\"result\":{result}}}");
    let mut stdout = io::stdout().lock();
    write!(stdout, "Content-Length: {}\r\n\r\n{body}", body.len())?;
    stdout.flush()
}

fn bump(path: &std::path::Path) -> io::Result<()> {
    let count = fs::read_to_string(path)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(0)
        .saturating_add(1);
    fs::write(path, count.to_string())
}

fn stem_contains(needle: &str) -> bool {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.file_stem().map(|stem| stem.to_string_lossy().into_owned()))
        .is_some_and(|stem| stem.contains(needle))
}

fn main() -> io::Result<()> {
    let executable = std::env::current_exe()?;
    let count_path = executable.with_extension("count");
    let cancelled_path = executable.with_extension("cancelled");
    let opened_path = executable.with_extension("opened");
    let last_open_path = executable.with_extension("lastopen");
    let changed_path = executable.with_extension("changed");
    let watched_path = executable.with_extension("watched");
    let child_path = executable.with_extension("child");
    bump(&count_path)?;
    let hang = stem_contains("hang");
    let crash = stem_contains("crash");
    let no_capabilities = stem_contains("no-capabilities");
    let spawn_child = stem_contains("spawn-child");
    let cross_document = stem_contains("cross-document");
    let _child: Option<Child> = if spawn_child {
        let child = std::process::Command::new("sh")
            .args(["-c", "while :; do :; done"])
            .spawn()?;
        fs::write(&child_path, child.id().to_string())?;
        Some(child)
    } else {
        None
    };
    let stdin = io::stdin();
    let mut stdin = stdin.lock();
    let mut last_uri = String::new();
    loop {
        let mut content_length = None;
        loop {
            let mut header = String::new();
            if stdin.read_line(&mut header)? == 0 {
                return Ok(());
            }
            let header = header.trim_end_matches(['\r', '\n']);
            if header.is_empty() {
                break;
            }
            if let Some(value) = header.strip_prefix("Content-Length: ") {
                content_length = value.parse::<usize>().ok();
            }
        }
        let Some(content_length) = content_length else {
            return Ok(());
        };
        let mut body = vec![0; content_length];
        stdin.read_exact(&mut body)?;
        let body = String::from_utf8_lossy(&body);
        if body.contains("\"method\":\"initialize\"") {
            let capabilities = if no_capabilities {
                "{\"capabilities\":{}}"
            } else {
                "{\"capabilities\":{\"definitionProvider\":true,\"referencesProvider\":true,\"documentSymbolProvider\":true}}"
            };
            if let Some(id) = request_id(&body) {
                send(id, capabilities)?;
            }
        } else if body.contains("\"method\":\"textDocument/didOpen\"") {
            bump(&opened_path)?;
            fs::write(&last_open_path, body.as_bytes())?;
            if let Some(uri) = request_uri(&body) {
                last_uri = uri.to_owned();
            }
        } else if body.contains("\"method\":\"textDocument/didChange\"") {
            bump(&changed_path)?;
        } else if body.contains("\"method\":\"workspace/didChangeWatchedFiles\"") {
            bump(&watched_path)?;
        } else if body.contains("\"method\":\"textDocument/references\"") {
            if crash {
                std::process::exit(17);
            }
            if hang {
                continue;
            }
            if let Some(uri) = request_uri(&body) {
                last_uri = uri.to_owned();
            }
            let reference_uri = if cross_document {
                last_uri
                    .strip_suffix("main.tf")
                    .map_or_else(|| last_uri.clone(), |prefix| format!("{prefix}other.tf"))
            } else {
                last_uri.clone()
            };
            let references = format!(
                "[{{\"uri\":\"{reference_uri}\",\"range\":{{\"start\":{{\"line\":3,\"character\":13}},\"end\":{{\"line\":3,\"character\":33}}}}}}]"
            );
            if let Some(id) = request_id(&body) {
                send(id, &references)?;
            }
        } else if body.contains("\"method\":\"textDocument/documentSymbol\"") {
            if hang {
                continue;
            }
            let symbols = format!(
                "[{{\"name\":\"null_resource.caller\",\"kind\":19,\"location\":{{\"uri\":\"{last_uri}\",\"range\":{{\"start\":{{\"line\":1,\"character\":0}},\"end\":{{\"line\":5,\"character\":1}}}}}},\"containerName\":null}}]"
            );
            if let Some(id) = request_id(&body) {
                send(id, &symbols)?;
            }
        } else if body.contains("\"method\":\"$/cancelRequest\"") {
            fs::write(&cancelled_path, body.as_bytes())?;
        } else if body.contains("\"method\":\"shutdown\"") {
            if let Some(id) = request_id(&body) {
                send(id, "null")?;
            }
        } else if body.contains("\"method\":\"exit\"") {
            return Ok(());
        }
    }
}
"#;

fn compile_fake_server(root: &Path, name: &str) -> Result<PathBuf, Box<dyn Error>> {
    let source = root.join(format!("{name}.rs"));
    let executable = root.join(if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_owned()
    });
    fs::write(&source, FAKE_SERVER)?;
    let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| OsString::from("rustc"));
    let status = Command::new(rustc)
        .args(["--edition=2024", "-o"])
        .arg(&executable)
        .arg(&source)
        .status()?;
    if !status.success() {
        return Err("failed to compile the fake terraform_ls peer".into());
    }
    Ok(executable)
}

struct SharedFakeServer {
    _scratch: tempfile::TempDir,
    executable: PathBuf,
}

fn materialize_fake_server(root: &Path, name: &str) -> Result<PathBuf, Box<dyn Error>> {
    static SERVER: OnceLock<Result<SharedFakeServer, String>> = OnceLock::new();
    let shared = match SERVER.get_or_init(|| {
        let scratch = tempfile::tempdir().map_err(|error| error.to_string())?;
        let executable = compile_fake_server(scratch.path(), "fake-terraform_ls-shared")
            .map_err(|error| error.to_string())?;
        Ok(SharedFakeServer {
            _scratch: scratch,
            executable,
        })
    }) {
        Ok(server) => &server.executable,
        Err(message) => return Err(std::io::Error::other(message.clone()).into()),
    };
    let executable = root.join(if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_owned()
    });
    fs::copy(shared, &executable)?;
    Ok(executable)
}

fn wait_for_file(path: &Path) -> Result<String, Box<dyn Error>> {
    // The scripted peer writes marker files asynchronously; poll with a
    // bounded deadline instead of assuming the write has landed.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match fs::read_to_string(path) {
            Ok(contents) => return Ok(contents),
            Err(error) => {
                if Instant::now() >= deadline {
                    return Err(error.into());
                }
                thread::sleep(Duration::from_millis(25));
            }
        }
    }
}

const TARGET_SOURCE: &str = "resource \"null_resource\" \"target\" {}\nresource \"null_resource\" \"caller\" {\n  triggers = {\n    target = null_resource.target.id\n  }\n}\n";

fn document(path: &RepoRelativePath, source: &str) -> ProviderDocument {
    ProviderDocument {
        path: path.clone(),
        source: Arc::from(source),
        language: Language::Hcl,
    }
}

fn workspace(
    root: &Path,
    revision: Revision,
    documents: Vec<ProviderDocument>,
) -> Result<ProviderWorkspace, Box<dyn Error>> {
    Ok(ProviderWorkspace::from_documents(
        fs::canonicalize(root)?,
        revision,
        documents,
    ))
}

fn request(root: &Path, revision: Revision) -> Result<PreciseQueryRequest, Box<dyn Error>> {
    let path = RepoRelativePath::new("main.tf")?;
    fs::create_dir_all(root.join("src"))?;
    fs::write(root.join(path.as_str()), TARGET_SOURCE)?;
    Ok(PreciseQueryRequest {
        workspace: workspace(root, revision, vec![document(&path, TARGET_SOURCE)])?,
        symbol: ProviderSymbol {
            name: "target".to_owned(),
            declaration: SourceRange::new(
                path,
                TextPosition::new(1, 1)?,
                TextPosition::new(1, 37)?,
            )?,
            language: Language::Hcl,
        },
        directions: CallHierarchyDirections {
            incoming: true,
            outgoing: false,
        },
        limit: 20,
        priority: chakra_engine::ProviderRequestPriority::Normal,
    })
}

fn config(executable: &Path) -> TerraformLsConfig {
    TerraformLsConfig {
        command: TerraformLsCommand::start(executable.as_os_str().to_owned()),
        startup_timeout: Duration::from_secs(5),
        request_timeout: Duration::from_millis(500),
        barrier_timeout: Duration::from_millis(250),
        query_wait_timeout: Duration::from_secs(10),
        restart_base_delay: Duration::from_millis(20),
        restart_max_delay: Duration::from_millis(100),
        ..TerraformLsConfig::default()
    }
}

fn counter(executable: &Path, extension: &str) -> String {
    fs::read_to_string(executable.with_extension(extension)).unwrap_or_else(|_| "0".to_owned())
}

#[test]
fn precise_incoming_call_carries_terraform_ls_provenance() -> Result<(), Box<dyn Error>> {
    let repository = tempfile::tempdir()?;
    let executable = materialize_fake_server(repository.path(), "fake-terraform_ls-precise")?;
    let request = request(repository.path(), Revision(1))?;
    let provider = TerraformLsProvider::start(request.workspace.clone(), config(&executable))?;

    let result = provider.enrich(request);
    assert_eq!(
        result.state,
        ProviderState::Ready,
        "last_error={:?}",
        provider.last_error()
    );
    assert_eq!(result.revision, Revision(1));
    assert_eq!(result.incoming.len(), 1, "incoming: {:?}", result.incoming);
    let relation = &result.incoming[0];
    assert_eq!(relation.name, "caller");
    assert_eq!(relation.provenance, Provenance::TerraformLs);
    assert_eq!(relation.occurrence_count, 1);
    assert_eq!(relation.call_sites.len(), 1);
    assert_eq!(provider.state_for(Revision(1)), ProviderState::Ready);
    provider.shutdown()?;
    Ok(())
}

#[test]
fn terraform_document_syncs_with_the_terraform_language_id() -> Result<(), Box<dyn Error>> {
    let repository = tempfile::tempdir()?;
    let executable = materialize_fake_server(repository.path(), "fake-terraform-ls-hcl")?;
    let path = RepoRelativePath::new("main.tf")?;
    fs::create_dir_all(repository.path().join("src"))?;
    fs::write(repository.path().join(path.as_str()), TARGET_SOURCE)?;
    let request = PreciseQueryRequest {
        workspace: workspace(
            repository.path(),
            Revision(1),
            vec![ProviderDocument {
                path: path.clone(),
                source: Arc::from(TARGET_SOURCE),
                language: Language::Hcl,
            }],
        )?,
        symbol: ProviderSymbol {
            name: "target".to_owned(),
            declaration: SourceRange::new(
                path,
                TextPosition::new(1, 1)?,
                TextPosition::new(1, 37)?,
            )?,
            language: Language::Hcl,
        },
        directions: CallHierarchyDirections {
            incoming: true,
            outgoing: false,
        },
        limit: 20,
        priority: chakra_engine::ProviderRequestPriority::Normal,
    };
    let provider = TerraformLsProvider::start(request.workspace.clone(), config(&executable))?;

    let result = provider.enrich(request);
    assert_eq!(
        result.state,
        ProviderState::Ready,
        "last_error={:?}",
        provider.last_error()
    );
    assert_eq!(result.incoming.len(), 1);
    assert_eq!(result.incoming[0].provenance, Provenance::TerraformLs);
    let opened = wait_for_file(&executable.with_extension("lastopen"))?;
    assert!(
        opened.contains("\"languageId\":\"terraform\""),
        "didOpen must carry the terraform language id: {opened}"
    );
    provider.shutdown()?;
    Ok(())
}

#[test]
fn referenced_documents_are_opened_before_document_symbol_requests() -> Result<(), Box<dyn Error>> {
    let repository = tempfile::tempdir()?;
    let executable =
        materialize_fake_server(repository.path(), "fake-terraform_ls-cross-document")?;
    let mut request = request(repository.path(), Revision(1))?;
    let main_path = RepoRelativePath::new("main.tf")?;
    let other_path = RepoRelativePath::new("other.tf")?;
    let json_path = RepoRelativePath::new("generated.tf.json")?;
    fs::write(repository.path().join(other_path.as_str()), TARGET_SOURCE)?;
    fs::write(
        repository.path().join(json_path.as_str()),
        r#"{"resource":{"null_resource":{"json_only":{}}}}"#,
    )?;
    request.workspace = workspace(
        repository.path(),
        Revision(1),
        vec![
            document(&main_path, TARGET_SOURCE),
            document(&other_path, TARGET_SOURCE),
            document(
                &json_path,
                r#"{"resource":{"null_resource":{"json_only":{}}}}"#,
            ),
        ],
    )?;
    let provider = TerraformLsProvider::start(request.workspace.clone(), config(&executable))?;

    // Assert the constructor-time observability before any query-side delta
    // recalculates it: Terraform JSON is not a terraform-ls input (#113).
    let deadline = Instant::now() + Duration::from_secs(5);
    let initial_metrics = loop {
        if let Some(metrics) = provider.metrics()
            && metrics.document_sync.revision == Some(Revision(1))
        {
            break metrics;
        }
        if Instant::now() >= deadline {
            return Err("initial provider metrics were not published".into());
        }
        thread::sleep(Duration::from_millis(5));
    };
    assert_eq!(initial_metrics.document_sync.workspace_documents, 2);

    let result = provider.enrich(request);
    assert_eq!(
        result.state,
        ProviderState::Ready,
        "last_error={:?}",
        provider.last_error()
    );
    assert_eq!(result.incoming.len(), 1, "incoming: {:?}", result.incoming);
    assert_eq!(result.incoming[0].name, "caller");
    assert_eq!(counter(&executable, "opened"), "2");
    let metrics = provider.metrics().ok_or("provider metrics unavailable")?;
    assert_eq!(metrics.document_sync.workspace_documents, 2);
    assert_eq!(metrics.document_sync.opened_documents, 2);
    assert_eq!(metrics.document_sync.total_text_documents_sent, 2);
    provider.shutdown()?;
    Ok(())
}

#[test]
fn revision_delta_syncs_only_opened_documents_with_full_text() -> Result<(), Box<dyn Error>> {
    let repository = tempfile::tempdir()?;
    let executable = materialize_fake_server(repository.path(), "fake-terraform_ls-delta")?;
    let mut request = request(repository.path(), Revision(1))?;
    let other_path = RepoRelativePath::new("other.tf")?;
    fs::write(
        repository.path().join(other_path.as_str()),
        "resource \"null_resource\" \"other\" {}\n",
    )?;
    request.workspace = workspace(
        repository.path(),
        Revision(1),
        vec![
            document(&RepoRelativePath::new("main.tf")?, TARGET_SOURCE),
            document(&other_path, "resource \"null_resource\" \"other\" {}\n"),
        ],
    )?;
    let provider = TerraformLsProvider::start(request.workspace.clone(), config(&executable))?;

    let result = provider.enrich(request.clone());
    assert_eq!(
        result.state,
        ProviderState::Ready,
        "last_error={:?}",
        provider.last_error()
    );
    assert_eq!(
        counter(&executable, "opened"),
        "1",
        "only the target document is opened with full text"
    );

    // A revision changing only an unopened document produces watched-file
    // events and no full-text traffic.
    let revision_two = PreciseQueryRequest {
        workspace: workspace(
            repository.path(),
            Revision(2),
            vec![
                document(&RepoRelativePath::new("main.tf")?, TARGET_SOURCE),
                document(
                    &other_path,
                    "resource \"null_resource\" \"other\" { triggers = { changed = true } }\n",
                ),
            ],
        )?,
        ..request.clone()
    };
    let result = provider.enrich(revision_two);
    assert_eq!(result.state, ProviderState::Ready);
    assert_eq!(result.revision, Revision(2));
    assert_eq!(counter(&executable, "opened"), "1");
    assert_eq!(counter(&executable, "changed"), "0");
    assert_eq!(counter(&executable, "watched"), "1");

    // A revision changing the opened target document sends didChange.
    let changed_target: Arc<str> = Arc::from(format!("{TARGET_SOURCE}# edit\n"));
    let revision_three = PreciseQueryRequest {
        workspace: workspace(
            repository.path(),
            Revision(3),
            vec![
                ProviderDocument {
                    path: RepoRelativePath::new("main.tf")?,
                    source: changed_target,
                    language: Language::Hcl,
                },
                document(
                    &other_path,
                    "resource \"null_resource\" \"other\" { triggers = { changed = true } }\n",
                ),
            ],
        )?,
        ..request
    };
    let result = provider.enrich(revision_three);
    assert_eq!(result.state, ProviderState::Ready);
    assert_eq!(
        fs::read_to_string(executable.with_extension("changed"))?,
        "1"
    );
    let metrics = provider.metrics().ok_or("provider metrics unavailable")?;
    assert_eq!(metrics.document_sync.revision, Some(Revision(3)));
    assert_eq!(metrics.document_sync.total_text_documents_sent, 2);
    provider.shutdown()?;
    Ok(())
}

#[test]
fn timed_out_request_is_cancelled_and_reports_catching_up() -> Result<(), Box<dyn Error>> {
    let repository = tempfile::tempdir()?;
    let executable = materialize_fake_server(repository.path(), "fake-terraform_ls-hang")?;
    let request = request(repository.path(), Revision(1))?;
    let provider = TerraformLsProvider::start(request.workspace.clone(), config(&executable))?;

    let result = provider.enrich(request);
    assert_eq!(result.state, ProviderState::CatchingUp);
    provider.shutdown()?;
    let cancellation = wait_for_file(&executable.with_extension("cancelled"))?;
    assert!(cancellation.contains("$/cancelRequest"));
    Ok(())
}

#[test]
fn per_query_wait_budget_returns_before_the_request_timeout() -> Result<(), Box<dyn Error>> {
    let repository = tempfile::tempdir()?;
    let executable = materialize_fake_server(repository.path(), "fake-terraform_ls-hang-budget")?;
    let request = request(repository.path(), Revision(1))?;
    let mut bounded = config(&executable);
    bounded.request_timeout = Duration::from_secs(2);
    bounded.query_wait_timeout = Duration::from_millis(100);
    let provider = TerraformLsProvider::start(request.workspace.clone(), bounded)?;

    let started = Instant::now();
    let result = provider.enrich(request);
    let elapsed = started.elapsed();
    assert_eq!(result.state, ProviderState::CatchingUp);
    // Nominal bound is the 100ms query wait budget; keep a generous ceiling
    // below the 2s request timeout for heavily loaded hosts.
    assert!(elapsed < Duration::from_millis(1500), "elapsed={elapsed:?}");
    provider.shutdown()?;
    Ok(())
}

#[test]
fn transport_crash_restarts_once_then_degrades() -> Result<(), Box<dyn Error>> {
    let repository = tempfile::tempdir()?;
    let executable = materialize_fake_server(repository.path(), "fake-terraform_ls-crash")?;
    let request = request(repository.path(), Revision(1))?;
    let provider = TerraformLsProvider::start(request.workspace.clone(), config(&executable))?;

    let result = provider.enrich(request);
    let process_count = wait_for_file(&executable.with_extension("count"))?;
    assert_eq!(
        result.state,
        ProviderState::Degraded,
        "last_error={:?}, process_count={process_count}",
        provider.last_error()
    );
    assert_eq!(process_count, "2", "one restart attempt after the crash");
    assert_eq!(provider.state_for(Revision(1)), ProviderState::Degraded);
    assert!(provider.last_error().is_some());
    provider.shutdown()?;
    Ok(())
}

#[test]
fn missing_navigation_capabilities_degrades() -> Result<(), Box<dyn Error>> {
    let repository = tempfile::tempdir()?;
    let executable =
        materialize_fake_server(repository.path(), "fake-terraform-ls-no-capabilities")?;
    let request = request(repository.path(), Revision(1))?;
    let provider = TerraformLsProvider::start(request.workspace.clone(), config(&executable))?;

    let result = provider.enrich(request);
    assert_eq!(result.state, ProviderState::Degraded);
    assert!(
        provider
            .last_error()
            .is_some_and(|error| error.contains("definition, references, and document symbols")),
        "last_error={:?}",
        provider.last_error()
    );
    provider.shutdown()?;
    Ok(())
}

#[cfg(unix)]
#[test]
fn shutdown_reaps_provider_process_group_descendants() -> Result<(), Box<dyn Error>> {
    let repository = tempfile::tempdir()?;
    let executable = materialize_fake_server(repository.path(), "fake-terraform_ls-spawn-child")?;
    let request = request(repository.path(), Revision(1))?;
    let provider = TerraformLsProvider::start(request.workspace.clone(), config(&executable))?;
    let result = provider.enrich(request);
    assert_eq!(result.state, ProviderState::Ready);
    let child = wait_for_file(&executable.with_extension("child"))?;
    provider.shutdown()?;

    // The descendant is killed asynchronously and may linger as a zombie
    // until the init reaper runs; poll with a bounded deadline.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let status = Command::new("kill")
            .args(["-0", child.trim()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?;
        if !status.success() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "provider descendant {child} is still alive"
        );
        thread::sleep(Duration::from_millis(25));
    }
    Ok(())
}
