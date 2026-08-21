//! Hermetic rust-analyzer lifecycle regressions using a tiny stdio LSP peer.

use std::error::Error;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use chakra_domain::location::{RepoRelativePath, SourceRange, TextPosition};
use chakra_domain::operation::OperationContext;
use chakra_domain::revision::Revision;
use chakra_domain::state::ProviderState;
use chakra_engine::{
    CallHierarchyDirections, PreciseProvider, PreciseQueryRequest, ProviderDocument,
    ProviderSymbol, ProviderWorkspace,
};
use chakra_provider_rust_analyzer::{RustAnalyzerConfig, RustAnalyzerProvider};

const FAKE_SERVER: &str = r#"
use std::fs;
use std::io::{self, BufRead, Read, Write};
use std::path::PathBuf;
use std::process::{Child, Command};

fn request_id(body: &str) -> Option<&str> {
    let rest = body.split_once("\"id\":")?.1;
    let end = rest.find(|character: char| !character.is_ascii_digit())?;
    rest.get(..end)
}

fn send(id: &str, result: &str) -> io::Result<()> {
    let body = format!("{{\"jsonrpc\":\"2.0\",\"id\":{id},\"result\":{result}}}");
    let mut stdout = io::stdout().lock();
    write!(stdout, "Content-Length: {}\r\n\r\n{body}", body.len())?;
    stdout.flush()
}

fn notify(method: &str, params: &str) -> io::Result<()> {
    let body = format!("{{\"jsonrpc\":\"2.0\",\"method\":\"{method}\",\"params\":{params}}}");
    let mut stdout = io::stdout().lock();
    write!(stdout, "Content-Length: {}\r\n\r\n{body}", body.len())?;
    stdout.flush()
}

fn main() -> io::Result<()> {
    let executable = std::env::current_exe()?;
    let count_path = executable.with_extension("count");
    let count = fs::read_to_string(&count_path)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(0)
        .saturating_add(1);
    fs::write(&count_path, count.to_string())?;
    let hang = executable
        .file_stem()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.contains("hang"));
    let no_read = executable
        .file_stem()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.contains("no-read"));
    let record_open = executable
        .file_stem()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.contains("record-open"));
    let spawn_child = executable
        .file_stem()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.contains("spawn-child"));
    let cancelled_path: PathBuf = executable.with_extension("cancelled");
    let opened_path: PathBuf = executable.with_extension("opened");
    let prepared_path: PathBuf = executable.with_extension("prepared");
    let child_path: PathBuf = executable.with_extension("child");
    let _child: Option<Child> = if spawn_child {
        let child = Command::new("sh")
            .args(["-c", "while :; do :; done"])
            .spawn()?;
        fs::write(&child_path, child.id().to_string())?;
        Some(child)
    } else {
        None
    };
    let stdin = io::stdin();
    let mut stdin = stdin.lock();

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
            send("1", "{\"capabilities\":{\"callHierarchyProvider\":true}}")?;
            if no_read {
                loop {
                    std::thread::park();
                }
            }
        } else if body.contains("\"method\":\"initialized\"") && record_open {
            notify(
                "experimental/serverStatus",
                "{\"health\":\"ok\",\"quiescent\":true,\"message\":null}",
            )?;
        } else if body.contains("\"method\":\"textDocument/didOpen\"") && record_open {
            let opened = fs::read_to_string(&opened_path)
                .ok()
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or(0)
                .saturating_add(1);
            fs::write(&opened_path, opened.to_string())?;
        } else if body.contains("\"method\":\"textDocument/prepareCallHierarchy\"") {
            fs::write(&prepared_path, body.as_bytes())?;
            if record_open {
                if let Some(id) = request_id(&body) {
                    send(id, "[]")?;
                }
            } else if !hang {
                std::process::exit(17);
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
        return Err("failed to compile the fake rust-analyzer peer".into());
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
        let executable = compile_fake_server(scratch.path(), "fake-ra-shared")
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

fn request(root: &Path, revision: Revision) -> Result<PreciseQueryRequest, Box<dyn Error>> {
    let path = RepoRelativePath::new("src/lib.rs")?;
    let source: Arc<str> = Arc::from("pub fn target() {}\n");
    fs::create_dir_all(root.join("src"))?;
    fs::write(root.join(path.as_str()), source.as_ref())?;
    Ok(PreciseQueryRequest {
        workspace: ProviderWorkspace::from_documents(
            fs::canonicalize(root)?,
            revision,
            vec![ProviderDocument {
                path: path.clone(),
                source,
                language: chakra_domain::symbol::Language::Rust,
            }],
        ),
        symbol: ProviderSymbol {
            name: "target".to_owned(),
            declaration: SourceRange::new(
                path,
                TextPosition::new(1, 1)?,
                TextPosition::new(1, 19)?,
            )?,
            language: chakra_domain::symbol::Language::Rust,
        },
        directions: CallHierarchyDirections {
            incoming: true,
            outgoing: false,
        },
        limit: 20,
        priority: chakra_engine::ProviderRequestPriority::Normal,
    })
}

fn zed_scale_documents() -> Result<Vec<ProviderDocument>, Box<dyn Error>> {
    let large_source: Arc<str> = Arc::from(format!("//{}\n", "x".repeat(28_688)));
    let mut documents = vec![
        ProviderDocument {
            path: RepoRelativePath::new("src/lib.rs")?,
            source: Arc::from("pub fn target() {}\n"),
            language: chakra_domain::symbol::Language::Rust,
        },
        ProviderDocument {
            path: RepoRelativePath::new("src/caller.rs")?,
            source: large_source.clone(),
            language: chakra_domain::symbol::Language::Rust,
        },
    ];
    for index in 2..1_929 {
        documents.push(ProviderDocument {
            path: RepoRelativePath::new(format!("src/generated_{index}.rs"))?,
            source: large_source.clone(),
            language: chakra_domain::symbol::Language::Rust,
        });
    }
    Ok(documents)
}

fn config(executable: &Path) -> RustAnalyzerConfig {
    RustAnalyzerConfig {
        executable: executable.as_os_str().to_owned(),
        startup_timeout: Duration::from_secs(5),
        request_timeout: Duration::from_secs(1),
        barrier_timeout: Duration::from_millis(250),
        query_wait_timeout: Duration::from_secs(10),
        ..RustAnalyzerConfig::default()
    }
}

#[test]
fn transport_crash_restarts_once_then_degrades() -> Result<(), Box<dyn Error>> {
    let repository = tempfile::tempdir()?;
    let executable = materialize_fake_server(repository.path(), "fake-ra-crash")?;
    let mut request = request(repository.path(), Revision(1))?;
    request.workspace = ProviderWorkspace::from_documents(
        fs::canonicalize(repository.path())?,
        Revision(1),
        zed_scale_documents()?,
    );
    let provider = RustAnalyzerProvider::start(request.workspace.clone(), config(&executable))?;

    let result = provider.enrich(request.clone());
    let process_count = fs::read_to_string(executable.with_extension("count"))?;
    assert_eq!(
        result.state,
        ProviderState::Degraded,
        "last_error={:?}, process_count={process_count}",
        provider.last_error()
    );
    assert_eq!(process_count, "2");
    assert_eq!(provider.state_for(Revision(1)), ProviderState::Degraded);
    assert!(provider.last_error().is_some());
    provider.shutdown()?;
    Ok(())
}

#[test]
fn timed_out_request_is_cancelled_before_shutdown() -> Result<(), Box<dyn Error>> {
    let repository = tempfile::tempdir()?;
    let executable = materialize_fake_server(repository.path(), "fake-ra-hang")?;
    let request = request(repository.path(), Revision(1))?;
    let provider = RustAnalyzerProvider::start(request.workspace.clone(), config(&executable))?;

    let result = provider.enrich(request.clone());
    assert_eq!(result.state, ProviderState::CatchingUp);
    provider.shutdown()?;
    let cancellation = fs::read_to_string(executable.with_extension("cancelled"))?;
    assert!(cancellation.contains("$/cancelRequest"));
    assert!(cancellation.contains("\"id\":2"));
    Ok(())
}

#[test]
fn per_query_wait_budget_returns_catching_up_before_request_timeout() -> Result<(), Box<dyn Error>>
{
    let repository = tempfile::tempdir()?;
    let executable = materialize_fake_server(repository.path(), "fake-ra-hang-wait-budget")?;
    let request = request(repository.path(), Revision(1))?;
    let mut bounded = config(&executable);
    bounded.request_timeout = Duration::from_secs(2);
    bounded.query_wait_timeout = Duration::from_millis(75);
    let provider = RustAnalyzerProvider::start(request.workspace.clone(), bounded)?;

    let started = Instant::now();
    let result = provider.enrich(request);
    let elapsed = started.elapsed();
    assert_eq!(result.state, ProviderState::CatchingUp);
    assert!(elapsed < Duration::from_millis(250), "elapsed={elapsed:?}");
    assert_eq!(
        provider.query_wait_budget(),
        Some(Duration::from_millis(75))
    );
    provider.shutdown()?;
    Ok(())
}

#[test]
fn caller_cancellation_interrupts_an_in_flight_request() -> Result<(), Box<dyn Error>> {
    let repository = tempfile::tempdir()?;
    let executable = materialize_fake_server(repository.path(), "fake-ra-hang-cancel")?;
    let mut request = request(repository.path(), Revision(1))?;
    request.workspace = ProviderWorkspace::from_documents(
        fs::canonicalize(repository.path())?,
        Revision(1),
        zed_scale_documents()?,
    );
    let provider = RustAnalyzerProvider::start(request.workspace.clone(), config(&executable))?;
    let operation = OperationContext::unbounded();
    let worker_operation = operation.clone();
    let worker_provider = provider.clone();
    let (completed, result) = mpsc::sync_channel(1);
    let query = std::thread::spawn(move || {
        let response = worker_provider.enrich_with_context(request, &worker_operation);
        let _ = completed.send(response);
    });

    let marker = executable.with_extension("prepared");
    let marker_deadline = Instant::now() + Duration::from_secs(5);
    while !marker.exists() {
        if Instant::now() >= marker_deadline {
            return Err("fake provider did not receive the precise request".into());
        }
        std::thread::yield_now();
    }
    operation.cancel();
    let response = result
        .recv_timeout(Duration::from_millis(250))
        .map_err(|_| "cancelled provider request did not return promptly")?;
    assert_eq!(response.state, ProviderState::CatchingUp);
    query.join().map_err(|_| "provider query thread panicked")?;
    provider.shutdown()?;
    let cancellation = fs::read_to_string(executable.with_extension("cancelled"))?;
    assert!(cancellation.contains("$/cancelRequest"));
    Ok(())
}

#[test]
fn blocked_provider_stdin_is_bounded_and_shutdown_completes() -> Result<(), Box<dyn Error>> {
    let repository = tempfile::tempdir()?;
    let executable = materialize_fake_server(repository.path(), "fake-ra-no-read")?;
    let mut request = request(repository.path(), Revision(1))?;
    let source: Arc<str> = Arc::from(format!(
        "pub fn target() {{}}\n// {}\n",
        "x".repeat(2 * 1024 * 1024)
    ));
    fs::write(repository.path().join("src/lib.rs"), source.as_ref())?;
    request.workspace = ProviderWorkspace::from_documents(
        fs::canonicalize(repository.path())?,
        Revision(1),
        vec![ProviderDocument {
            path: RepoRelativePath::new("src/lib.rs")?,
            source,
            language: chakra_domain::symbol::Language::Rust,
        }],
    );
    let provider = RustAnalyzerProvider::start(request.workspace.clone(), config(&executable))?;

    let result = provider.enrich(request);
    assert_eq!(
        result.state,
        ProviderState::Degraded,
        "last_error={:?}",
        provider.last_error()
    );
    assert!(
        provider
            .last_error()
            .is_some_and(|error| error.contains("timed out writing"))
    );
    provider.shutdown()?;
    Ok(())
}

#[cfg(unix)]
#[test]
fn shutdown_reaps_provider_process_group_descendants() -> Result<(), Box<dyn Error>> {
    let repository = tempfile::tempdir()?;
    let executable = materialize_fake_server(repository.path(), "fake-ra-record-open-spawn-child")?;
    let request = request(repository.path(), Revision(1))?;
    let provider = RustAnalyzerProvider::start(request.workspace.clone(), config(&executable))?;
    let result = provider.enrich(request);
    assert_eq!(result.state, ProviderState::Ready);
    let child = fs::read_to_string(executable.with_extension("child"))?;
    provider.shutdown()?;

    let status = Command::new("kill")
        .args(["-0", child.trim()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    assert!(
        !status.success(),
        "provider descendant {child} is still alive"
    );
    Ok(())
}

#[test]
fn zed_scale_inventory_opens_only_target_and_measures_revision_delta() -> Result<(), Box<dyn Error>>
{
    let repository = tempfile::tempdir()?;
    let executable = materialize_fake_server(repository.path(), "fake-ra-record-open")?;
    let mut request = request(repository.path(), Revision(1))?;
    let second_path = RepoRelativePath::new("src/caller.rs")?;
    let documents = zed_scale_documents()?;
    let caller_source = documents
        .iter()
        .find(|document| document.path == second_path)
        .ok_or("caller document missing")?
        .source
        .clone();
    fs::write(
        repository.path().join(second_path.as_str()),
        caller_source.as_ref(),
    )?;
    let mut revision_two_documents = documents.clone();
    request.workspace = ProviderWorkspace::from_documents(
        fs::canonicalize(repository.path())?,
        Revision(1),
        documents,
    );
    let provider = RustAnalyzerProvider::start(request.workspace.clone(), config(&executable))?;

    let result = provider.enrich(request.clone());
    assert_eq!(
        result.state,
        ProviderState::Ready,
        "last_error={:?}",
        provider.last_error()
    );
    assert_eq!(
        fs::read_to_string(executable.with_extension("opened"))?,
        "1"
    );
    let metrics = provider.metrics().ok_or("provider metrics unavailable")?;
    assert_eq!(metrics.document_sync.workspace_documents, 1_929);
    assert_eq!(metrics.document_sync.workspace_source_bytes, 55_316_267);
    assert_eq!(metrics.document_sync.text_documents_sent, 1);
    assert_eq!(metrics.document_sync.text_bytes_sent, 19);

    let changed_second: Arc<str> = Arc::from("pub fn caller() { crate::target(); } // changed\n");
    fs::write(
        repository.path().join("src/caller.rs"),
        changed_second.as_ref(),
    )?;
    let caller = revision_two_documents
        .iter_mut()
        .find(|document| document.path.as_str() == "src/caller.rs")
        .ok_or("caller document missing")?;
    caller.source = changed_second;
    let revision_two = PreciseQueryRequest {
        workspace: ProviderWorkspace::from_documents(
            fs::canonicalize(repository.path())?,
            Revision(2),
            revision_two_documents,
        ),
        ..request
    };
    let changed = provider.enrich(revision_two.clone());
    assert_eq!(changed.state, ProviderState::Ready);
    let metrics = provider.metrics().ok_or("provider metrics unavailable")?;
    assert_eq!(metrics.document_sync.revision, Some(Revision(2)));
    assert_eq!(metrics.document_sync.changed, 1);
    assert_eq!(metrics.document_sync.text_documents_sent, 0);
    assert_eq!(metrics.document_sync.watched_file_events, 1);
    assert_eq!(metrics.document_sync.source_body_comparisons, 1);
    assert_eq!(metrics.document_sync.total_text_documents_sent, 1);

    let cached = provider.enrich(revision_two);
    assert_eq!(cached.state, ProviderState::Ready);
    let metrics = provider.metrics().ok_or("provider metrics unavailable")?;
    assert_eq!(metrics.cache.hits, 1);
    assert_eq!(metrics.document_sync.total_text_documents_sent, 1);
    provider.shutdown()?;
    Ok(())
}
