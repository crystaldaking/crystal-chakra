use std::ffi::OsStr;
use std::io::{BufReader, BufWriter, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError, SendTimeoutError, Sender, bounded};
use lsp_server::Message;
use thiserror::Error;

const INCOMING_MESSAGE_CAPACITY: usize = 64;
const OUTGOING_MESSAGE_CAPACITY: usize = 8;
const READER_SEND_POLL: Duration = Duration::from_millis(50);
const PROCESS_EXIT_GRACE: Duration = Duration::from_millis(500);

#[derive(Debug, Error)]
pub(crate) enum TransportError {
    #[error("failed to start rust-analyzer: {0}")]
    Spawn(#[source] std::io::Error),
    #[error("rust-analyzer stdin is unavailable")]
    MissingStdin,
    #[error("rust-analyzer stdout is unavailable")]
    MissingStdout,
    #[error("rust-analyzer stderr is unavailable")]
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
    #[error("rust-analyzer output closed: {0}")]
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

pub(crate) struct Session {
    child: Child,
    outgoing: Option<Sender<WriteCommand>>,
    incoming: Receiver<TransportEvent>,
    stopping: Arc<AtomicBool>,
    writer: Option<JoinHandle<()>>,
    reader: Option<JoinHandle<()>>,
    stderr: Option<JoinHandle<()>>,
}

impl Session {
    pub(crate) fn spawn(executable: &OsStr, root: &Path) -> Result<Self, TransportError> {
        let mut child = Command::new(executable)
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(TransportError::Spawn)?;
        let Some(stdin) = child.stdin.take() else {
            terminate_unowned_child(&mut child);
            return Err(TransportError::MissingStdin);
        };
        let Some(stdout) = child.stdout.take() else {
            terminate_unowned_child(&mut child);
            return Err(TransportError::MissingStdout);
        };
        let Some(stderr) = child.stderr.take() else {
            terminate_unowned_child(&mut child);
            return Err(TransportError::MissingStderr);
        };
        let (sender, incoming) = bounded(INCOMING_MESSAGE_CAPACITY);
        let (outgoing, writer_commands) = bounded::<WriteCommand>(OUTGOING_MESSAGE_CAPACITY);
        let stopping = Arc::new(AtomicBool::new(false));
        let writer_stopping = stopping.clone();
        let writer = match thread::Builder::new()
            .name("chakra-ra-stdin".to_owned())
            .spawn(move || {
                let mut stdin = BufWriter::new(stdin);
                while !writer_stopping.load(Ordering::Acquire) {
                    let command = match writer_commands.recv_timeout(READER_SEND_POLL) {
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
                terminate_unowned_child(&mut child);
                return Err(TransportError::ThreadSpawn {
                    name: "stdin writer",
                    source,
                });
            }
        };
        let reader_stopping = stopping.clone();
        let reader = match thread::Builder::new()
            .name("chakra-ra-stdout".to_owned())
            .spawn(move || {
                let mut stdout = BufReader::new(stdout);
                loop {
                    match Message::read(&mut stdout) {
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
                                TransportEvent::Closed(error.to_string()),
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
                terminate_unowned_child(&mut child);
                drop(outgoing);
                let _ = writer.join();
                return Err(TransportError::ThreadSpawn {
                    name: "stdout reader",
                    source,
                });
            }
        };
        let stderr = match thread::Builder::new()
            .name("chakra-ra-stderr".to_owned())
            .spawn(move || {
                use std::io::BufRead;

                for line in BufReader::new(stderr).lines() {
                    match line {
                        Ok(line) => tracing::debug!(target: "rust_analyzer", message = %line),
                        Err(error) => {
                            tracing::debug!(target: "rust_analyzer", %error, "stderr closed");
                            break;
                        }
                    }
                }
            }) {
            Ok(stderr) => stderr,
            Err(source) => {
                stopping.store(true, Ordering::Release);
                terminate_unowned_child(&mut child);
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
    /// the worker before this transport-level fallback runs.
    pub(crate) fn terminate(&mut self) {
        self.stopping.store(true, Ordering::Release);
        self.outgoing.take();
        let deadline = Instant::now() + PROCESS_EXIT_GRACE;
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(10));
                }
                Ok(None) | Err(_) => {
                    let _ = self.child.kill();
                    let _ = self.child.wait();
                    break;
                }
            }
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

impl Drop for Session {
    fn drop(&mut self) {
        self.terminate();
    }
}

fn send_while_running(
    sender: &Sender<TransportEvent>,
    mut event: TransportEvent,
    stopping: &AtomicBool,
) -> bool {
    while !stopping.load(Ordering::Acquire) {
        match sender.send_timeout(event, READER_SEND_POLL) {
            Ok(()) => return true,
            Err(SendTimeoutError::Timeout(returned)) => event = returned,
            Err(SendTimeoutError::Disconnected(_)) => return false,
        }
    }
    false
}

fn terminate_unowned_child(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}
