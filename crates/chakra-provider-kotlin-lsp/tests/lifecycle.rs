//! Hermetic kotlin-lsp lifecycle regressions using a tiny scripted stdio LSP peer.

use std::error::Error;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use chakra_domain::location::{RepoRelativePath, SourceRange, TextPosition};
use chakra_domain::operation::OperationContext;
use chakra_domain::provenance::Provenance;
use chakra_domain::revision::Revision;
use chakra_domain::state::ProviderState;
use chakra_domain::symbol::Language;
use chakra_engine::{
    CallHierarchyDirections, PreciseProvider, PreciseQueryRequest, ProviderDocument,
    ProviderSymbol, ProviderWorkspace,
};
use chakra_provider_kotlin_lsp::{KotlinLspCommand, KotlinLspConfig, KotlinLspProvider};

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

fn progress(kind: &str) -> io::Result<()> {
    let body = format!("{{\"jsonrpc\":\"2.0\",\"method\":\"$/progress\",\"params\":{{\"token\":\"indexing\",\"value\":{{\"kind\":\"{kind}\",\"title\":\"Indexing\"}}}}}}");
    let mut stdout = io::stdout().lock();
    write!(stdout, "Content-Length: {}\r\n\r\n{body}", body.len())?;
    stdout.flush()
}

fn import_status(status: &str) -> io::Result<()> {
    let body = format!("{{\"jsonrpc\":\"2.0\",\"method\":\"intellij/workspaceImportState\",\"params\":{{\"phase\":\"FINISHED\",\"folders\":[{{\"status\":\"{status}\"}}]}}}}");
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
    // Match the pinned standalone launcher's CLI contract, so lifecycle
    // tests reject positional `stdio` and duplicated transport arguments.
    if std::env::args().skip(1).collect::<Vec<_>>() != ["--stdio"] {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "expected --stdio"));
    }
    let executable = std::env::current_exe()?;
    let count_path = executable.with_extension("count");
    let cancelled_path = executable.with_extension("cancelled");
    let opened_path = executable.with_extension("opened");
    let changed_path = executable.with_extension("changed");
    let watched_path = executable.with_extension("watched");
    let prepared_path = executable.with_extension("prepared");
    let child_path = executable.with_extension("child");
    bump(&count_path)?;
    let hang = stem_contains("hang");
    let crash = stem_contains("crash");
    let no_hierarchy = stem_contains("no-hierarchy");
    let delayed_import = stem_contains("delayed-import");
    let no_item = stem_contains("no-item");
    let no_calls = stem_contains("no-calls");
    let delayed_calls = stem_contains("delayed-calls");
    let indexing = stem_contains("indexing");
    let stuck_indexing = stem_contains("stuck-indexing");
    let spawn_child = stem_contains("spawn-child");
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
    let mut prepares = 0;
    let mut index_finished_at = None;
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
            fs::write(executable.with_extension("initialize"), body.as_bytes())?;
            let capabilities = if no_hierarchy {
                "{\"capabilities\":{\"definitionProvider\":true,\"referencesProvider\":true}}"
            } else if stem_contains("no-references") {
                "{\"capabilities\":{\"callHierarchyProvider\":true,\"definitionProvider\":true}}"
            } else if stem_contains("external-model") {
                "{\"capabilities\":{\"definitionProvider\":true,\"referencesProvider\":true}}"
            } else {
                "{\"capabilities\":{\"callHierarchyProvider\":true,\"definitionProvider\":true,\"referencesProvider\":true}}"
            };
            if let Some(id) = request_id(&body) {
                send(id, capabilities)?;
            }
        } else if body.contains("\"method\":\"initialized\"") {
            progress("begin")?;
            progress("end")?;
            if !stem_contains("missing-import") {
                import_status(if stem_contains("failed-import") { "FAILED" } else { "SUCCESS" })?;
            }
        } else if body.contains("\"method\":\"textDocument/didOpen\"") {
            bump(&opened_path)?;
            if let Some(uri) = request_uri(&body) {
                last_uri = uri.to_owned();
            }
        } else if body.contains("\"method\":\"textDocument/didChange\"") {
            bump(&changed_path)?;
            prepares = 0;
        } else if body.contains("\"method\":\"workspace/didChangeWatchedFiles\"") {
            bump(&watched_path)?;
        } else if body.contains("\"method\":\"textDocument/prepareCallHierarchy\"") {
            prepares += 1;
            bump(&prepared_path)?;
            if crash {
                std::process::exit(17);
            }
            if hang {
                continue;
            }
            if no_item || (delayed_import && prepares <= 3) {
                if let Some(id) = request_id(&body) {
                    send(id, "null")?;
                }
                continue;
            }
            if indexing && prepares == 1 {
                progress("begin")?;
                if !stuck_indexing {
                    index_finished_at = Some(std::time::Instant::now() + std::time::Duration::from_millis(600));
                    std::thread::spawn(|| {
                        std::thread::sleep(std::time::Duration::from_millis(600));
                        progress("end").expect("write indexing completion");
                    });
                }
            }
            if let Some(uri) = request_uri(&body) {
                last_uri = uri.to_owned();
            }
            let item = format!(
                "[{{\"name\":\"target\",\"kind\":12,\"uri\":\"{last_uri}\",\"range\":{{\"start\":{{\"line\":2,\"character\":0}},\"end\":{{\"line\":2,\"character\":16}}}},\"selectionRange\":{{\"start\":{{\"line\":2,\"character\":5}},\"end\":{{\"line\":2,\"character\":11}}}}}}]"
            );
            if let Some(id) = request_id(&body) {
                send(id, &item)?;
            }
        } else if body.contains("\"method\":\"callHierarchy/incomingCalls\"") {
            if hang {
                continue;
            }
            if delayed_calls {
                std::thread::sleep(std::time::Duration::from_millis(300));
            }
            if no_calls || (indexing && index_finished_at.is_none_or(|end| std::time::Instant::now() < end)) {
                if let Some(id) = request_id(&body) {
                    send(id, "[]")?;
                }
                continue;
            }
            let call = format!(
                "[{{\"from\":{{\"name\":\"caller\",\"kind\":12,\"uri\":\"{last_uri}\",\"range\":{{\"start\":{{\"line\":3,\"character\":0}},\"end\":{{\"line\":3,\"character\":25}}}},\"selectionRange\":{{\"start\":{{\"line\":3,\"character\":5}},\"end\":{{\"line\":3,\"character\":11}}}}}},\"fromRanges\":[{{\"start\":{{\"line\":3,\"character\":16}},\"end\":{{\"line\":3,\"character\":22}}}}]}}]"
            );
            if let Some(id) = request_id(&body) {
                send(id, &call)?;
            }
        } else if body.contains("\"method\":\"textDocument/definition\"") {
            if let Some(id) = request_id(&body) {
                let result = format!("{{\"uri\":\"{last_uri}\",\"range\":{{\"start\":{{\"line\":2,\"character\":4}},\"end\":{{\"line\":2,\"character\":10}}}}}}");
                send(id, &result)?;
            }
        } else if body.contains("\"method\":\"textDocument/references\"") {
            if let Some(id) = request_id(&body) {
                let result = format!("[{{\"uri\":\"{last_uri}\",\"range\":{{\"start\":{{\"line\":3,\"character\":15}},\"end\":{{\"line\":3,\"character\":21}}}}}}]");
                send(id, &result)?;
            }
        } else if body.contains("\"method\":\"callHierarchy/outgoingCalls\"") {
            if let Some(id) = request_id(&body) {
                send(id, "[]")?;
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
        return Err("failed to compile the fake kotlin-lsp peer".into());
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
        let executable = compile_fake_server(scratch.path(), "fake-kotlin-lsp-shared")
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
                std::thread::sleep(Duration::from_millis(25));
            }
        }
    }
}

const TARGET_SOURCE: &str = "package sample\n\nfun target() {}\nfun caller() { target() }\n";

#[test]
fn gradle_kotlin_dsl_queries_do_not_enter_the_kotlin_session() -> Result<(), Box<dyn Error>> {
    let repository = tempfile::tempdir()?;
    let executable =
        materialize_fake_server(repository.path(), "fake-kotlin-gradle-script-routing")?;
    let request = request(repository.path(), Revision(1))?;
    let provider = KotlinLspProvider::start(request.workspace.clone(), config(&executable))?;
    assert_eq!(provider.enrich(request.clone()).state, ProviderState::Ready);
    let prepared = counter(&executable, "prepared");
    for name in [
        "build.gradle.kts",
        "settings.gradle.kts",
        "gradle/conventions.gradle.kts",
    ] {
        let path = RepoRelativePath::new(name)?;
        let mut unsupported = request.clone();
        unsupported.symbol.declaration = SourceRange::new(
            path.clone(),
            TextPosition::new(3, 1)?,
            TextPosition::new(3, 13)?,
        )?;
        assert!(!provider.supports_path(Language::Kotlin, &path), "{name}");
        assert_eq!(provider.enrich(unsupported).state, ProviderState::Degraded);
        assert_eq!(counter(&executable, "prepared"), prepared);
        assert_eq!(provider.state_for(Revision(1)), ProviderState::Ready);
    }
    assert!(provider.supports_path(Language::Kotlin, &RepoRelativePath::new("service.kt")?));
    provider.shutdown()?;
    Ok(())
}

#[test]
fn companion_language_queries_do_not_enter_the_kotlin_session() -> Result<(), Box<dyn Error>> {
    let repository = tempfile::tempdir()?;
    let executable = materialize_fake_server(repository.path(), "fake-kotlin-companion-routing")?;
    let request = request(repository.path(), Revision(1))?;
    let provider = KotlinLspProvider::start(request.workspace.clone(), config(&executable))?;
    let result = provider.enrich(request.clone());
    assert_eq!(result.state, ProviderState::Ready);
    let prepared = counter(&executable, "prepared");
    let mut unsupported = request;
    unsupported.symbol.language = Language::Java;
    assert!(!provider.supports(Language::Java));
    assert_eq!(provider.enrich(unsupported).state, ProviderState::Degraded);
    assert_eq!(counter(&executable, "prepared"), prepared);
    assert_eq!(provider.state_for(Revision(1)), ProviderState::Ready);
    provider.shutdown()?;
    Ok(())
}

#[test]
fn missing_references_cannot_enable_incomplete_mixed_language_queries() -> Result<(), Box<dyn Error>>
{
    let repository = tempfile::tempdir()?;
    let executable = materialize_fake_server(repository.path(), "fake-kotlin-no-references")?;
    let request = request(repository.path(), Revision(1))?;
    let provider = KotlinLspProvider::start(request.workspace.clone(), config(&executable))?;
    assert_eq!(provider.enrich(request).state, ProviderState::Degraded);
    assert!(
        provider
            .last_error()
            .is_some_and(|error| error.contains("definitions and references"))
    );
    assert_eq!(counter(&executable, "prepared"), "0");
    provider.shutdown()?;
    Ok(())
}

fn document(path: &RepoRelativePath, source: &str) -> ProviderDocument {
    ProviderDocument {
        path: path.clone(),
        source: Arc::from(source),
        language: Language::Kotlin,
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
    let path = RepoRelativePath::new("service.kt")?;
    fs::write(root.join(path.as_str()), TARGET_SOURCE)?;
    Ok(PreciseQueryRequest {
        workspace: workspace(root, revision, vec![document(&path, TARGET_SOURCE)])?,
        symbol: ProviderSymbol {
            name: "target".to_owned(),
            declaration: SourceRange::new(
                path,
                TextPosition::new(3, 1)?,
                TextPosition::new(3, 16)?,
            )?,
            language: Language::Kotlin,
        },
        directions: CallHierarchyDirections {
            incoming: true,
            outgoing: false,
        },
        limit: 20,
        priority: chakra_engine::ProviderRequestPriority::Normal,
    })
}

fn config(executable: &Path) -> KotlinLspConfig {
    KotlinLspConfig {
        command: KotlinLspCommand::stdio(executable.as_os_str().to_owned()),
        startup_timeout: Duration::from_secs(5),
        readiness_timeout: Duration::from_millis(500),
        request_timeout: Duration::from_millis(500),
        barrier_timeout: Duration::from_millis(250),
        query_wait_timeout: Duration::from_secs(10),
        restart_base_delay: Duration::from_millis(20),
        restart_max_delay: Duration::from_millis(100),
        ..KotlinLspConfig::default()
    }
}

fn counter(executable: &Path, extension: &str) -> String {
    fs::read_to_string(executable.with_extension(extension)).unwrap_or_else(|_| "0".to_owned())
}

#[test]
fn precise_incoming_call_carries_kotlin_lsp_provenance() -> Result<(), Box<dyn Error>> {
    let repository = tempfile::tempdir()?;
    let executable = materialize_fake_server(repository.path(), "fake-kotlin-lsp-precise")?;
    let request = request(repository.path(), Revision(1))?;
    let provider = KotlinLspProvider::start(request.workspace.clone(), config(&executable))?;

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
    assert_eq!(relation.provenance, Provenance::KotlinLsp);
    assert_eq!(relation.occurrence_count, 1);
    assert_eq!(relation.call_sites.len(), 1);
    assert_eq!(provider.state_for(Revision(1)), ProviderState::Ready);
    provider.shutdown()?;
    Ok(())
}

#[test]
fn hierarchy_answers_during_indexing_are_discarded() -> Result<(), Box<dyn Error>> {
    let repository = tempfile::tempdir()?;
    let executable = materialize_fake_server(repository.path(), "fake-kotlin-indexing")?;
    let request = request(repository.path(), Revision(1))?;
    let mut settings = config(&executable);
    settings.readiness_timeout = Duration::from_secs(3);
    let provider = KotlinLspProvider::start(request.workspace.clone(), settings)?;
    let started = Instant::now();
    let result = provider.enrich(request);
    assert_eq!(result.state, ProviderState::Ready);
    assert_eq!(
        result.incoming.len(),
        1,
        "early empty answer must not be precise"
    );
    assert!(started.elapsed() >= Duration::from_millis(600));
    assert_eq!(counter(&executable, "prepared"), "2");
    provider.shutdown()?;
    Ok(())
}

#[test]
fn never_ending_indexing_is_bounded_and_never_ready() -> Result<(), Box<dyn Error>> {
    let repository = tempfile::tempdir()?;
    let executable = materialize_fake_server(repository.path(), "fake-kotlin-stuck-indexing")?;
    let request = request(repository.path(), Revision(1))?;
    let provider = KotlinLspProvider::start(request.workspace.clone(), config(&executable))?;
    let started = Instant::now();
    let result = provider.enrich(request);
    assert_eq!(result.state, ProviderState::CatchingUp);
    assert!(result.incoming.is_empty());
    assert!(started.elapsed() < Duration::from_secs(3));
    assert_ne!(provider.state_for(Revision(1)), ProviderState::Ready);
    provider.shutdown()?;
    Ok(())
}

#[test]
fn cold_incoming_calls_share_the_readiness_budget() -> Result<(), Box<dyn Error>> {
    let repository = tempfile::tempdir()?;
    let executable = materialize_fake_server(repository.path(), "fake-kotlin-delayed-calls")?;
    let request = request(repository.path(), Revision(1))?;
    let mut settings = config(&executable);
    settings.request_timeout = Duration::from_millis(100);
    settings.readiness_timeout = Duration::from_secs(3);
    let provider = KotlinLspProvider::start(request.workspace.clone(), settings)?;
    let result = provider.enrich(request);
    assert_eq!(result.state, ProviderState::Ready);
    assert_eq!(result.incoming.len(), 1);
    provider.shutdown()?;
    Ok(())
}

#[test]
fn null_prepare_waits_for_import_without_publishing_ready() -> Result<(), Box<dyn Error>> {
    let repository = tempfile::tempdir()?;
    let executable = materialize_fake_server(repository.path(), "fake-kotlin-delayed-import")?;
    let initial = request(repository.path(), Revision(1))?;
    let mut settings = config(&executable);
    settings.readiness_timeout = Duration::from_secs(3);
    let provider = KotlinLspProvider::start(initial.workspace.clone(), settings)?;
    let worker_provider = provider.clone();
    let first = initial.clone();
    let query = std::thread::spawn(move || worker_provider.enrich(first));
    wait_for_file(&executable.with_extension("prepared"))?;
    assert_eq!(provider.state_for(Revision(1)), ProviderState::CatchingUp);
    let result = query.join().map_err(|_| "query thread panicked")?;
    assert_eq!(result.state, ProviderState::Ready);
    assert_eq!(result.incoming.len(), 1);
    assert_eq!(counter(&executable, "prepared"), "4");

    // A new document generation must not inherit the old readiness proof.
    let changed = format!("{TARGET_SOURCE}// changed\n");
    let path = RepoRelativePath::new("service.kt")?;
    fs::write(repository.path().join(path.as_str()), &changed)?;
    let result = provider.enrich(PreciseQueryRequest {
        workspace: workspace(
            repository.path(),
            Revision(2),
            vec![document(&path, &changed)],
        )?,
        ..initial
    });
    assert_eq!(result.state, ProviderState::Ready);
    assert_eq!(result.revision, Revision(2));
    assert_eq!(result.incoming.len(), 1);
    assert_eq!(counter(&executable, "prepared"), "8");
    provider.shutdown()?;
    Ok(())
}

#[test]
fn cold_import_finishes_after_the_callers_short_wait() -> Result<(), Box<dyn Error>> {
    let repository = tempfile::tempdir()?;
    let executable =
        materialize_fake_server(repository.path(), "fake-kotlin-delayed-import-budget")?;
    let request = request(repository.path(), Revision(1))?;
    let mut settings = config(&executable);
    settings.readiness_timeout = Duration::from_secs(3);
    settings.query_wait_timeout = Duration::from_millis(100);
    let provider = KotlinLspProvider::start(request.workspace.clone(), settings)?;
    assert_eq!(
        provider.enrich(request.clone()).state,
        ProviderState::CatchingUp
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while provider.state_for(Revision(1)) != ProviderState::Ready {
        assert!(Instant::now() < deadline, "cold import never became ready");
        std::thread::sleep(Duration::from_millis(10));
    }
    let result = provider.enrich(request);
    assert_eq!(result.state, ProviderState::Ready);
    assert_eq!(result.incoming.len(), 1);
    assert_eq!(counter(&executable, "count"), "1");
    provider.shutdown()?;
    Ok(())
}

#[test]
fn missing_hierarchy_item_has_a_bounded_readiness_deadline() -> Result<(), Box<dyn Error>> {
    let repository = tempfile::tempdir()?;
    let executable = materialize_fake_server(repository.path(), "fake-kotlin-no-item")?;
    let request = request(repository.path(), Revision(1))?;
    let mut settings = config(&executable);
    settings.readiness_timeout = Duration::from_millis(150);
    let provider = KotlinLspProvider::start(request.workspace.clone(), settings)?;
    let started = Instant::now();
    assert_eq!(provider.enrich(request).state, ProviderState::CatchingUp);
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(provider.state_for(Revision(1)), ProviderState::CatchingUp);
    assert!(
        provider
            .last_error()
            .is_some_and(|message| message.contains("timed out"))
    );
    provider.shutdown()?;
    Ok(())
}

#[test]
fn caller_cancellation_interrupts_the_import_retry_wait() -> Result<(), Box<dyn Error>> {
    let repository = tempfile::tempdir()?;
    let executable = materialize_fake_server(repository.path(), "fake-kotlin-no-item-cancel")?;
    let request = request(repository.path(), Revision(1))?;
    let mut settings = config(&executable);
    settings.readiness_timeout = Duration::from_secs(5);
    let provider = KotlinLspProvider::start(request.workspace.clone(), settings)?;
    let operation = OperationContext::unbounded();
    let query_operation = operation.clone();
    let query_provider = provider.clone();
    let query =
        std::thread::spawn(move || query_provider.enrich_with_context(request, &query_operation));
    wait_for_file(&executable.with_extension("prepared"))?;
    let started = Instant::now();
    operation.cancel();
    assert_eq!(
        query.join().map_err(|_| "query thread panicked")?.state,
        ProviderState::CatchingUp
    );
    provider.shutdown()?;
    assert!(started.elapsed() < Duration::from_secs(2));
    Ok(())
}

#[test]
fn hierarchy_item_with_no_callers_is_a_ready_empty_result() -> Result<(), Box<dyn Error>> {
    let repository = tempfile::tempdir()?;
    let executable = materialize_fake_server(repository.path(), "fake-kotlin-no-calls")?;
    let request = request(repository.path(), Revision(1))?;
    let provider = KotlinLspProvider::start(request.workspace.clone(), config(&executable))?;
    let result = provider.enrich(request);
    assert_eq!(result.state, ProviderState::Ready);
    assert!(result.incoming.is_empty());
    assert_eq!(counter(&executable, "prepared"), "1");
    provider.shutdown()?;
    Ok(())
}

#[test]
fn revision_delta_syncs_only_opened_documents_with_full_text() -> Result<(), Box<dyn Error>> {
    let repository = tempfile::tempdir()?;
    let executable = materialize_fake_server(repository.path(), "fake-kotlin-lsp-delta")?;
    let mut request = request(repository.path(), Revision(1))?;
    let other_path = RepoRelativePath::new("other.kt")?;
    fs::write(
        repository.path().join(other_path.as_str()),
        "package sample\nval value = 1\n",
    )?;
    request.workspace = workspace(
        repository.path(),
        Revision(1),
        vec![
            document(&RepoRelativePath::new("service.kt")?, TARGET_SOURCE),
            document(&other_path, "package sample\nval value = 1\n"),
        ],
    )?;
    let provider = KotlinLspProvider::start(request.workspace.clone(), config(&executable))?;

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
                document(&RepoRelativePath::new("service.kt")?, TARGET_SOURCE),
                document(&other_path, "package sample\nval value = 2\n"),
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
    let changed_target: Arc<str> =
        Arc::from("package sample\n\nfun target() {}\nfun caller() { target() }\n// edit\n");
    let revision_three = PreciseQueryRequest {
        workspace: workspace(
            repository.path(),
            Revision(3),
            vec![
                ProviderDocument {
                    path: RepoRelativePath::new("service.kt")?,
                    source: changed_target,
                    language: Language::Kotlin,
                },
                document(&other_path, "package sample\nval value = 2\n"),
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
    let executable = materialize_fake_server(repository.path(), "fake-kotlin-lsp-hang")?;
    let request = request(repository.path(), Revision(1))?;
    let provider = KotlinLspProvider::start(request.workspace.clone(), config(&executable))?;

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
    let executable = materialize_fake_server(repository.path(), "fake-kotlin-lsp-hang-budget")?;
    let request = request(repository.path(), Revision(1))?;
    let mut bounded = config(&executable);
    bounded.request_timeout = Duration::from_secs(2);
    bounded.query_wait_timeout = Duration::from_millis(100);
    let provider = KotlinLspProvider::start(request.workspace.clone(), bounded)?;

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
    let executable = materialize_fake_server(repository.path(), "fake-kotlin-lsp-crash")?;
    let request = request(repository.path(), Revision(1))?;
    let provider = KotlinLspProvider::start(request.workspace.clone(), config(&executable))?;

    let result = provider.enrich(request);
    let process_count = wait_for_file(&executable.with_extension("count"))?;
    let prepare_count = wait_for_file(&executable.with_extension("prepared"))?;
    assert_eq!(
        result.state,
        ProviderState::Degraded,
        "last_error={:?}, process_count={process_count}, prepare_count={prepare_count}",
        provider.last_error()
    );
    assert_eq!(
        prepare_count, "2",
        "one retry of the crash-inducing request"
    );
    assert_eq!(provider.state_for(Revision(1)), ProviderState::Degraded);
    assert!(provider.last_error().is_some());
    provider.shutdown()?;
    Ok(())
}

#[test]
fn missing_call_hierarchy_capability_degrades() -> Result<(), Box<dyn Error>> {
    let repository = tempfile::tempdir()?;
    let executable = materialize_fake_server(repository.path(), "fake-kotlin-lsp-no-hierarchy")?;
    let request = request(repository.path(), Revision(1))?;
    let provider = KotlinLspProvider::start(request.workspace.clone(), config(&executable))?;

    let result = provider.enrich(request);
    assert_eq!(result.state, ProviderState::Degraded);
    assert!(
        provider
            .last_error()
            .is_some_and(|error| error.contains("call hierarchy")),
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
    let executable = materialize_fake_server(repository.path(), "fake-kotlin-lsp-spawn-child")?;
    let request = request(repository.path(), Revision(1))?;
    let provider = KotlinLspProvider::start(request.workspace.clone(), config(&executable))?;
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
        std::thread::sleep(Duration::from_millis(25));
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn external_model_changes_import_root_but_preserves_canonical_source_uris()
-> Result<(), Box<dyn Error>> {
    external_model_coverage(false)
}

#[cfg(unix)]
#[test]
fn incomplete_model_cannot_claim_complete_query_coverage() -> Result<(), Box<dyn Error>> {
    external_model_coverage(true)
}

#[cfg(unix)]
fn external_model_coverage(incomplete: bool) -> Result<(), Box<dyn Error>> {
    let repository = tempfile::tempdir()?;
    use std::os::unix::fs::PermissionsExt;
    let root = fs::canonicalize(repository.path())?;
    fs::write(root.join("build.gradle.kts"), "// hermetic fixture")?;
    fs::write(
        root.join("fixture-model.json"),
        serde_json::to_vec(&serde_json::json!({
            "modules":[{"contentRoots":[{"sourceRoots":[{"path":root}]}]}]
        }))?,
    )?;
    let wrapper = root.join("gradlew");
    fs::write(
        &wrapper,
        "#!/bin/sh\nfor arg in \"$@\"; do\ncase \"$arg\" in -Dchakra.kotlin.modelDir=*) model=${arg#*=} ;; esac\ndone\ncp fixture-model.json \"$model/workspace.json\"\n",
    )?;
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o700))?;
    if incomplete {
        let mut script = fs::read_to_string(&wrapper)?;
        script.push_str("touch \"$model/incomplete-coverage\"\n");
        fs::write(&wrapper, script)?;
    }
    let executable = materialize_fake_server(repository.path(), "fake-kotlin-external-model")?;
    let mut request = request(repository.path(), Revision(4))?;
    request.directions.outgoing = true;
    let provider = KotlinLspProvider::start(request.workspace.clone(), config(&executable))?;
    let result = provider.enrich(request);
    assert_eq!(
        result.state,
        ProviderState::Ready,
        "{:?}",
        provider.last_error()
    );
    assert_eq!(result.revision, Revision(4));
    assert_eq!(result.incoming.len(), 1);
    assert_eq!(result.incoming_truncated, incomplete);
    assert_eq!(result.outgoing_truncated, incomplete);
    assert_eq!(result.incoming[0].declaration.file().as_str(), "service.kt");
    assert_eq!(result.incoming[0].name, "caller");
    let initialize: serde_json::Value =
        serde_json::from_str(&wait_for_file(&executable.with_extension("initialize"))?)?;
    let actual = initialize["params"]["rootUri"]
        .as_str()
        .ok_or("missing import root")?;
    let model_root = url::Url::parse(actual)?
        .to_file_path()
        .map_err(|_| "invalid import root")?;
    assert!(!model_root.starts_with(&root));
    assert!(model_root.join("workspace.json").is_file());
    assert_eq!(
        initialize["params"]["workspaceFolders"][0]["uri"].as_str(),
        Some(actual)
    );
    provider.shutdown()?;
    Ok(())
}

#[test]
fn failed_import_cannot_publish_precise_empty_or_nonempty_calls() -> Result<(), Box<dyn Error>> {
    let repository = tempfile::tempdir()?;
    let executable = materialize_fake_server(repository.path(), "fake-kotlin-failed-import")?;
    let request = request(repository.path(), Revision(1))?;
    let provider = KotlinLspProvider::start(request.workspace.clone(), config(&executable))?;
    let result = provider.enrich(request);
    assert_eq!(result.state, ProviderState::Degraded);
    assert!(result.incoming.is_empty());
    assert_eq!(counter(&executable, "prepared"), "0");
    assert!(
        provider
            .last_error()
            .is_some_and(|error| error.contains("project import failed"))
    );
    provider.shutdown()?;
    Ok(())
}

#[test]
fn missing_import_completion_is_bounded_catching_up() -> Result<(), Box<dyn Error>> {
    let repository = tempfile::tempdir()?;
    let executable = materialize_fake_server(repository.path(), "fake-kotlin-missing-import")?;
    let request = request(repository.path(), Revision(1))?;
    let provider = KotlinLspProvider::start(request.workspace.clone(), config(&executable))?;
    let result = provider.enrich(request);
    assert_eq!(result.state, ProviderState::CatchingUp);
    assert!(result.incoming.is_empty());
    assert_eq!(counter(&executable, "prepared"), "0");
    provider.shutdown()?;
    Ok(())
}
