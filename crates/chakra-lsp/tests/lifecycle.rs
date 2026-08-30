//! Hermetic chakra-lsp lifecycle tests using a tiny scripted stdio LSP peer.

use std::error::Error;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use chakra_lsp::{
    Client, ClientConfig, ClientError, Health, ServerEvent, TransportConfig, TransportError,
};

const FAKE_SERVER: &str = r#"
use std::fs;
use std::io::{self, BufRead, Read, Write};
use std::process::Child;

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

fn stem_contains(needle: &str) -> bool {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.file_stem().map(|stem| stem.to_string_lossy().into_owned()))
        .is_some_and(|stem| stem.contains(needle))
}

fn main() -> io::Result<()> {
    let executable = std::env::current_exe()?;
    let cancelled_path = executable.with_extension("cancelled");
    let child_path = executable.with_extension("child");
    let hang = stem_contains("hang");
    let big = stem_contains("big");
    let spawn_child = stem_contains("spawn-child");
    let ignore_shutdown = stem_contains("ignore-shutdown");
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
    let mut announced_big = false;
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
            if let Some(id) = request_id(&body) {
                send(id, "{\"capabilities\":{\"callHierarchyProvider\":true}}")?;
            }
            if big && !announced_big {
                announced_big = true;
                let payload = format!("{{\"jsonrpc\":\"2.0\",\"method\":\"$/blob\",\"params\":\"{}\"}}", "x".repeat(2 * 1024 * 1024));
                let mut stdout = io::stdout().lock();
                // The client closes the transport on the oversized header, so
                // this write can hit EPIPE; the peer must survive to keep the
                // shutdown path deterministic.
                let _ = write!(stdout, "Content-Length: {}\r\n\r\n{payload}", payload.len());
                let _ = stdout.flush();
            }
        } else if body.contains("\"method\":\"shutdown\"") {
            if ignore_shutdown {
                continue;
            }
            if let Some(id) = request_id(&body) {
                send(id, "null")?;
            }
        } else if body.contains("\"method\":\"exit\"") {
            if ignore_shutdown {
                continue;
            }
            return Ok(());
        } else if body.contains("\"method\":\"$/cancelRequest\"") {
            fs::write(&cancelled_path, body.as_bytes())?;
        } else if body.contains("\"method\":\"test/triggerProgress\"") {
            notify("$/progress", "{\"token\":\"t\",\"value\":{\"kind\":\"begin\",\"title\":\"Loading\"}}")?;
            if let Some(id) = request_id(&body) {
                send(id, "{\"done\":true}")?;
            }
        } else if body.contains("\"method\":\"") && body.contains("\"id\":") && !hang {
            if let Some(id) = request_id(&body) {
                send(id, "{\"ok\":true}")?;
            }
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
        return Err("failed to compile the fake LSP peer".into());
    }
    Ok(executable)
}

struct SharedFakeServer {
    _scratch: tempfile::TempDir,
    executable: PathBuf,
}

fn shared_fake_server() -> Result<&'static Path, Box<dyn Error>> {
    static SERVER: OnceLock<Result<SharedFakeServer, String>> = OnceLock::new();

    match SERVER.get_or_init(|| {
        let scratch = tempfile::tempdir().map_err(|error| error.to_string())?;
        let executable = compile_fake_server(scratch.path(), "fake-lsp-shared")
            .map_err(|error| error.to_string())?;
        Ok(SharedFakeServer {
            _scratch: scratch,
            executable,
        })
    }) {
        Ok(server) => Ok(&server.executable),
        Err(message) => Err(std::io::Error::other(message.clone()).into()),
    }
}

fn materialize_fake_server(root: &Path, name: &str) -> Result<PathBuf, Box<dyn Error>> {
    let executable = root.join(if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_owned()
    });
    fs::copy(shared_fake_server()?, &executable)?;
    Ok(executable)
}

fn config() -> ClientConfig {
    ClientConfig {
        transport: TransportConfig {
            max_message_bytes: 1024 * 1024,
            ..TransportConfig::default()
        },
        startup_timeout: Duration::from_secs(5),
        shutdown_timeout: Duration::from_millis(500),
    }
}

fn spawn(executable: &Path, root: &Path) -> Result<Client, Box<dyn Error>> {
    // A freshly copied helper can still report ETXTBSY on a loaded host;
    // retry with a bounded deadline instead of failing the test.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match Client::spawn(executable.as_os_str(), &[], root, config(), "fake-lsp") {
            Ok(client) => return Ok(client),
            Err(ClientError::Transport(TransportError::Spawn(error)))
                if error.kind() == std::io::ErrorKind::ExecutableFileBusy
                    && Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(error) => return Err(error.into()),
        }
    }
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

fn initialize(client: &mut Client) -> Result<(), Box<dyn Error>> {
    client.initialize(&serde_json::json!({"capabilities": {}}), &mut |_| {})?;
    Ok(())
}

#[test]
fn handshake_and_echo_round_trip() -> Result<(), Box<dyn Error>> {
    let scratch = tempfile::tempdir()?;
    let executable = materialize_fake_server(scratch.path(), "fake-lsp-echo")?;
    let mut client = spawn(&executable, scratch.path())?;
    let initialize = client.initialize(&serde_json::json!({"capabilities": {}}), &mut |_| {})?;
    assert_eq!(
        initialize["capabilities"]["callHierarchyProvider"],
        serde_json::json!(true)
    );
    assert_eq!(client.health(), Health::Alive);
    let deadline = Instant::now() + Duration::from_secs(2);
    let result = client.request(
        "textDocument/definition",
        &serde_json::json!({}),
        deadline,
        None,
        &mut |_| {},
    )?;
    assert_eq!(result, serde_json::json!({"ok": true}));
    client.shutdown();
    Ok(())
}

#[test]
fn server_notifications_are_interleaved_while_waiting() -> Result<(), Box<dyn Error>> {
    let scratch = tempfile::tempdir()?;
    let executable = materialize_fake_server(scratch.path(), "fake-lsp-progress")?;
    let mut client = spawn(&executable, scratch.path())?;
    initialize(&mut client)?;
    let mut events = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(2);
    let result = client.request(
        "test/triggerProgress",
        &serde_json::json!({}),
        deadline,
        None,
        &mut |event| events.push(event),
    )?;
    assert_eq!(result, serde_json::json!({"done": true}));
    assert!(
        events.iter().any(|event| matches!(
            event,
            ServerEvent::Notification { method, .. } if method == "$/progress"
        )),
        "events: {events:?}"
    );
    client.shutdown();
    Ok(())
}

#[test]
fn timed_out_request_is_cancelled() -> Result<(), Box<dyn Error>> {
    let scratch = tempfile::tempdir()?;
    let executable = materialize_fake_server(scratch.path(), "fake-lsp-hang")?;
    let mut client = spawn(&executable, scratch.path())?;
    initialize(&mut client)?;
    let deadline = Instant::now() + Duration::from_millis(250);
    let result = client.request(
        "workspace/symbol",
        &serde_json::json!({}),
        deadline,
        None,
        &mut |_| {},
    );
    assert!(
        matches!(result, Err(ClientError::Timeout { .. })),
        "{result:?}"
    );
    client.shutdown();
    let cancellation = wait_for_file(&executable.with_extension("cancelled"))?;
    assert!(cancellation.contains("$/cancelRequest"));
    Ok(())
}

#[test]
fn caller_cancellation_is_forwarded() -> Result<(), Box<dyn Error>> {
    let scratch = tempfile::tempdir()?;
    let executable = materialize_fake_server(scratch.path(), "fake-lsp-hang-cancel")?;
    let mut client = spawn(&executable, scratch.path())?;
    initialize(&mut client)?;
    let cancelled = AtomicBool::new(true);
    let deadline = Instant::now() + Duration::from_secs(5);
    let result = client.request(
        "workspace/symbol",
        &serde_json::json!({}),
        deadline,
        Some(&|| cancelled.load(Ordering::Acquire)),
        &mut |_| {},
    );
    assert!(
        matches!(result, Err(ClientError::Cancelled { .. })),
        "{result:?}"
    );
    client.shutdown();
    let cancellation = wait_for_file(&executable.with_extension("cancelled"))?;
    assert!(cancellation.contains("$/cancelRequest"));
    Ok(())
}

#[test]
fn oversized_server_message_closes_the_transport() -> Result<(), Box<dyn Error>> {
    let scratch = tempfile::tempdir()?;
    let executable = materialize_fake_server(scratch.path(), "fake-lsp-big")?;
    let mut client = spawn(&executable, scratch.path())?;
    // The oversized blob follows the initialize response, so the handshake
    // completes; the next pump observes the bounded-read closure.
    client.initialize(&serde_json::json!({"capabilities": {}}), &mut |_| {})?;
    let deadline = Instant::now() + Duration::from_secs(2);
    let result = client.request(
        "textDocument/definition",
        &serde_json::json!({}),
        deadline,
        None,
        &mut |_| {},
    );
    assert!(
        matches!(result, Err(ClientError::Transport(_))),
        "error: {result:?}"
    );
    assert!(matches!(client.health(), Health::Closed(_)));
    client.shutdown();
    Ok(())
}

#[cfg(unix)]
#[test]
fn shutdown_reaps_process_group_descendants() -> Result<(), Box<dyn Error>> {
    let scratch = tempfile::tempdir()?;
    let executable = materialize_fake_server(scratch.path(), "fake-lsp-spawn-child")?;
    let mut client = spawn(&executable, scratch.path())?;
    initialize(&mut client)?;
    let child = wait_for_file(&executable.with_extension("child"))?;
    client.shutdown();
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
            "server descendant {child} is still alive"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
    Ok(())
}

#[test]
fn kill_fallback_terminates_a_server_that_ignores_shutdown() -> Result<(), Box<dyn Error>> {
    let scratch = tempfile::tempdir()?;
    let executable = materialize_fake_server(scratch.path(), "fake-lsp-ignore-shutdown")?;
    let mut client = spawn(&executable, scratch.path())?;
    initialize(&mut client)?;
    let deadline = Instant::now() + Duration::from_secs(2);
    client.request(
        "textDocument/definition",
        &serde_json::json!({}),
        deadline,
        None,
        &mut |_| {},
    )?;
    let started = Instant::now();
    client.shutdown();
    // Nominal bound is ~1.2s (shutdown handshake + exit grace + kill
    // fallback); keep a generous ceiling for heavily loaded hosts.
    assert!(
        started.elapsed() < Duration::from_secs(15),
        "shutdown took {:?}",
        started.elapsed()
    );
    Ok(())
}
