//! Owned stdio transport: one child process, three pump threads, bounded
//! framing and queues, and a process-group kill fallback.

use std::ffi::OsStr;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError, SendTimeoutError, Sender, bounded};
use lsp_server::Message;
use thiserror::Error;

#[cfg(unix)]
use nix::sys::signal::{Signal, killpg};
#[cfg(unix)]
use nix::unistd::Pid;
#[cfg(unix)]
use std::os::unix::process::CommandExt;

const PUMP_POLL: Duration = Duration::from_millis(50);
const HEADER_LIMIT: usize = 8 * 1024;

/// Bounded transport settings. All capacities are message counts; message
/// bodies are additionally capped by `max_message_bytes`.
#[derive(Debug, Clone)]
pub struct TransportConfig {
    pub incoming_capacity: usize,
    pub outgoing_capacity: usize,
    pub max_message_bytes: usize,
    pub exit_grace: Duration,
}

impl Default for TransportConfig {
    fn default() -> Self {
        Self {
            incoming_capacity: 64,
            outgoing_capacity: 8,
            max_message_bytes: 32 * 1024 * 1024,
            exit_grace: Duration::from_millis(500),
        }
    }
}

#[derive(Debug, Error)]
pub enum TransportError {
    #[error("invalid transport configuration: capacities and sizes must be non-zero")]
    InvalidConfig,
    #[error("failed to start the language server: {0}")]
    Spawn(#[source] std::io::Error),
    #[error("language server stdin is unavailable")]
    MissingStdin,
    #[error("language server stdout is unavailable")]
    MissingStdout,
    #[error("language server stderr is unavailable")]
    MissingStderr,
    #[error("failed to spawn {name} thread: {source}")]
    ThreadSpawn {
        name: &'static str,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to write an LSP message: {0}")]
    Write(String),
    #[error("timed out writing an LSP message")]
    WriteTimeout,
    #[error("language server output closed: {0}")]
    Closed(String),
}

#[derive(Debug)]
pub(crate) enum TransportEvent {
    Message(Message),
    Closed(String),
}

struct WriteCommand {
    bytes: Vec<u8>,
    completed: Sender<Result<(), String>>,
}

/// Owned child process and its pump threads. `Drop` terminates the child.
#[derive(Debug)]
pub(crate) struct Transport {
    child: Child,
    outgoing: Option<Sender<WriteCommand>>,
    incoming: Receiver<TransportEvent>,
    stopping: Arc<AtomicBool>,
    writer: Option<JoinHandle<()>>,
    reader: Option<JoinHandle<()>>,
    stderr: Option<JoinHandle<()>>,
    exit_grace: Duration,
}

impl Transport {
    pub(crate) fn spawn(
        program: &OsStr,
        args: &[&OsStr],
        root: &Path,
        config: &TransportConfig,
        log_target: &'static str,
    ) -> Result<Self, TransportError> {
        if config.incoming_capacity == 0
            || config.outgoing_capacity == 0
            || config.max_message_bytes == 0
        {
            return Err(TransportError::InvalidConfig);
        }
        let mut command = Command::new(program);
        command
            .args(args)
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        command.process_group(0);
        let mut child = command.spawn().map_err(TransportError::Spawn)?;
        let Some(stdin) = child.stdin.take() else {
            terminate_owned_process_tree(&mut child);
            return Err(TransportError::MissingStdin);
        };
        let Some(stdout) = child.stdout.take() else {
            terminate_owned_process_tree(&mut child);
            return Err(TransportError::MissingStdout);
        };
        let Some(stderr) = child.stderr.take() else {
            terminate_owned_process_tree(&mut child);
            return Err(TransportError::MissingStderr);
        };
        let (sender, incoming) = bounded(config.incoming_capacity);
        let (outgoing, writer_commands) = bounded::<WriteCommand>(config.outgoing_capacity);
        let stopping = Arc::new(AtomicBool::new(false));
        let writer_stopping = stopping.clone();
        let writer = match thread::Builder::new()
            .name("chakra-lsp-stdin".to_owned())
            .spawn(move || {
                let mut stdin = BufWriter::new(stdin);
                while !writer_stopping.load(Ordering::Acquire) {
                    let command = match writer_commands.recv_timeout(PUMP_POLL) {
                        Ok(command) => command,
                        Err(RecvTimeoutError::Timeout) => continue,
                        Err(RecvTimeoutError::Disconnected) => break,
                    };
                    let result = stdin
                        .write_all(&command.bytes)
                        .and_then(|()| stdin.flush())
                        .map_err(|error| error.to_string());
                    let failed = result.is_err();
                    let _ = command.completed.send(result);
                    if failed {
                        break;
                    }
                }
            }) {
            Ok(writer) => writer,
            Err(source) => {
                terminate_owned_process_tree(&mut child);
                return Err(TransportError::ThreadSpawn {
                    name: "stdin writer",
                    source,
                });
            }
        };
        let max_message_bytes = config.max_message_bytes;
        let reader_stopping = stopping.clone();
        let reader = match thread::Builder::new()
            .name("chakra-lsp-stdout".to_owned())
            .spawn(move || {
                let mut stdout = BufReader::new(stdout);
                loop {
                    match read_bounded(&mut stdout, max_message_bytes) {
                        Ok(Some(message)) => {
                            if !send_while_running(
                                &sender,
                                TransportEvent::Message(message),
                                &reader_stopping,
                            ) {
                                break;
                            }
                        }
                        Ok(None) => {
                            let _ = send_while_running(
                                &sender,
                                TransportEvent::Closed("end of stream".to_owned()),
                                &reader_stopping,
                            );
                            break;
                        }
                        Err(error) => {
                            let _ = send_while_running(
                                &sender,
                                TransportEvent::Closed(error),
                                &reader_stopping,
                            );
                            break;
                        }
                    }
                }
            }) {
            Ok(reader) => reader,
            Err(source) => {
                stopping.store(true, Ordering::Release);
                terminate_owned_process_tree(&mut child);
                drop(outgoing);
                let _ = writer.join();
                return Err(TransportError::ThreadSpawn {
                    name: "stdout reader",
                    source,
                });
            }
        };
        let stderr = match thread::Builder::new()
            .name("chakra-lsp-stderr".to_owned())
            .spawn(move || {
                for line in BufReader::new(stderr).lines() {
                    match line {
                        Ok(line) => {
                            tracing::debug!(target: "chakra_lsp", server = log_target, message = %line)
                        }
                        Err(error) => {
                            tracing::debug!(target: "chakra_lsp", server = log_target, %error, "stderr closed");
                            break;
                        }
                    }
                }
            }) {
            Ok(stderr) => stderr,
            Err(source) => {
                stopping.store(true, Ordering::Release);
                terminate_owned_process_tree(&mut child);
                drop(outgoing);
                let _ = writer.join();
                let _ = reader.join();
                return Err(TransportError::ThreadSpawn {
                    name: "stderr reader",
                    source,
                });
            }
        };
        Ok(Self {
            child,
            outgoing: Some(outgoing),
            incoming,
            stopping,
            writer: Some(writer),
            reader: Some(reader),
            stderr: Some(stderr),
            exit_grace: config.exit_grace,
        })
    }

    pub(crate) fn send(
        &mut self,
        message: &Message,
        deadline: Instant,
    ) -> Result<(), TransportError> {
        let outgoing = self.outgoing.as_ref().ok_or(TransportError::MissingStdin)?;
        let mut bytes = Vec::new();
        message
            .write(&mut bytes)
            .map_err(|error| TransportError::Write(error.to_string()))?;
        let (completed, result) = bounded(1);
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or(TransportError::WriteTimeout)?;
        outgoing
            .send_timeout(WriteCommand { bytes, completed }, remaining)
            .map_err(|error| match error {
                SendTimeoutError::Timeout(_) => TransportError::WriteTimeout,
                SendTimeoutError::Disconnected(_) => {
                    TransportError::Closed("stdin writer stopped".to_owned())
                }
            })?;
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or(TransportError::WriteTimeout)?;
        result
            .recv_timeout(remaining)
            .map_err(|error| match error {
                RecvTimeoutError::Timeout => TransportError::WriteTimeout,
                RecvTimeoutError::Disconnected => {
                    TransportError::Closed("stdin writer stopped".to_owned())
                }
            })?
            .map_err(TransportError::Write)
    }

    pub(crate) fn incoming(&self) -> &Receiver<TransportEvent> {
        &self.incoming
    }

    /// Ends only the owned child. A cooperative LSP shutdown is attempted by
    /// the client before this transport-level fallback runs. On Unix the
    /// whole owned process group is signaled so server-spawned descendants
    /// cannot remain orphaned.
    pub(crate) fn terminate(&mut self) {
        self.stopping.store(true, Ordering::Release);
        self.outgoing.take();
        let deadline = Instant::now() + self.exit_grace;
        let exited_cooperatively = loop {
            match self.child.try_wait() {
                Ok(Some(_)) => break true,
                Ok(None) if Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(10));
                }
                Ok(None) | Err(_) => {
                    break false;
                }
            }
        };
        if !exited_cooperatively {
            terminate_owned_process_tree(&mut self.child);
        } else {
            terminate_remaining_process_group(self.child.id());
        }
        if let Some(writer) = self.writer.take() {
            let _ = writer.join();
        }
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
        if let Some(stderr) = self.stderr.take() {
            let _ = stderr.join();
        }
    }
}

impl Drop for Transport {
    fn drop(&mut self) {
        self.terminate();
    }
}

/// Bounded LSP framing read. Headers are capped at [`HEADER_LIMIT`] bytes and
/// bodies at `max_message_bytes`; an oversized or malformed peer message
/// closes the transport instead of exhausting memory.
fn read_bounded(
    reader: &mut impl BufRead,
    max_message_bytes: usize,
) -> Result<Option<Message>, String> {
    let mut content_length: Option<usize> = None;
    let mut header_bytes = 0_usize;
    loop {
        let mut header = String::new();
        match reader.read_line(&mut header) {
            Ok(0) => return Ok(None),
            Ok(read) => {
                header_bytes = header_bytes.saturating_add(read);
                if header_bytes > HEADER_LIMIT {
                    return Err("LSP header section exceeded its bound".to_owned());
                }
            }
            Err(error) => return Err(error.to_string()),
        }
        let header = header.trim_end_matches(['\r', '\n']);
        if header.is_empty() {
            break;
        }
        if let Some(value) = header
            .split_once(':')
            .filter(|(name, _)| name.trim().eq_ignore_ascii_case("content-length"))
            .map(|(_, value)| value.trim())
        {
            content_length = value.parse::<usize>().ok();
        }
    }
    let Some(content_length) = content_length else {
        return Err("LSP message without a valid Content-Length header".to_owned());
    };
    if content_length > max_message_bytes {
        return Err(format!(
            "LSP message of {content_length} bytes exceeds the {max_message_bytes}-byte bound"
        ));
    }
    let mut body = vec![0_u8; content_length];
    reader
        .read_exact(&mut body)
        .map_err(|error| error.to_string())?;
    let message = serde_json::from_slice::<Message>(&body).map_err(|error| error.to_string())?;
    Ok(Some(message))
}

fn send_while_running(
    sender: &Sender<TransportEvent>,
    mut event: TransportEvent,
    stopping: &AtomicBool,
) -> bool {
    while !stopping.load(Ordering::Acquire) {
        match sender.send_timeout(event, PUMP_POLL) {
            Ok(()) => return true,
            Err(SendTimeoutError::Timeout(returned)) => event = returned,
            Err(SendTimeoutError::Disconnected(_)) => return false,
        }
    }
    false
}

#[cfg(unix)]
fn terminate_owned_process_tree(child: &mut Child) {
    let Ok(process_id) = i32::try_from(child.id()) else {
        let _ = child.kill();
        let _ = child.wait();
        return;
    };
    let group = Pid::from_raw(process_id);
    let _ = killpg(group, Signal::SIGTERM);
    let deadline = Instant::now() + Duration::from_millis(100);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(5)),
            Ok(None) | Err(_) => {
                let _ = killpg(group, Signal::SIGKILL);
                let _ = child.wait();
                break;
            }
        }
    }
    terminate_remaining_process_group(child.id());
}

#[cfg(unix)]
fn terminate_remaining_process_group(process_id: u32) {
    let Ok(process_id) = i32::try_from(process_id) else {
        return;
    };
    let group = Pid::from_raw(process_id);
    let _ = killpg(group, Signal::SIGTERM);
    thread::sleep(Duration::from_millis(10));
    let _ = killpg(group, Signal::SIGKILL);
}

#[cfg(not(unix))]
fn terminate_owned_process_tree(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(not(unix))]
fn terminate_remaining_process_group(_process_id: u32) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_read_rejects_an_oversized_body() {
        let input = b"Content-Length: 1048576\r\n\r\n";
        let mut slice = &input[..];
        assert!(
            read_bounded(&mut slice, 1024).is_err_and(|error| error.contains("exceeds")),
            "oversized body must be rejected"
        );
    }

    #[test]
    fn bounded_read_rejects_a_missing_content_length() {
        let input = b"Content-Type: application/vscode-jsonrpc; charset=utf-8\r\n\r\n{}";
        let mut slice = &input[..];
        assert!(
            read_bounded(&mut slice, 1024).is_err_and(|error| error.contains("Content-Length")),
            "missing Content-Length must be rejected"
        );
    }

    #[test]
    fn bounded_read_accepts_a_framed_notification() -> Result<(), Box<dyn std::error::Error>> {
        let body = b"{\"jsonrpc\":\"2.0\",\"method\":\"initialized\",\"params\":{}}";
        let framed = format!("Content-Length: {}\r\n\r\n", body.len());
        let mut input = framed.into_bytes();
        input.extend_from_slice(body);
        let mut slice = &input[..];
        let message = read_bounded(&mut slice, 1024)?.ok_or("message missing")?;
        assert!(matches!(message, Message::Notification(_)));
        Ok(())
    }
}
