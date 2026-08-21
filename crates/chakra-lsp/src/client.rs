//! Request/response routing, bounded initialization, cancellation, and
//! cooperative shutdown on top of the owned transport.

use std::ffi::OsStr;
use std::path::Path;
use std::time::{Duration, Instant};

use lsp_server::{ErrorCode, Message, Notification, Request, RequestId, Response, ResponseError};
use serde::Serialize;
use serde_json::Value;
use thiserror::Error;

use crate::transport::{Transport, TransportConfig, TransportError, TransportEvent};

const IDLE_POLL: Duration = Duration::from_millis(50);

/// Client settings. Every wait is bounded: initialization by
/// `startup_timeout`, the shutdown handshake and `$/cancelRequest` sends by
/// `shutdown_timeout`, and individual requests by the caller's deadline.
#[derive(Debug, Clone)]
pub struct ClientConfig {
    pub transport: TransportConfig,
    pub startup_timeout: Duration,
    pub shutdown_timeout: Duration,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            transport: TransportConfig::default(),
            startup_timeout: Duration::from_secs(15),
            shutdown_timeout: Duration::from_millis(750),
        }
    }
}

#[derive(Debug, Error)]
pub enum ClientError {
    #[error("client startup and shutdown timeouts must be non-zero")]
    InvalidConfig,
    #[error(transparent)]
    Transport(#[from] TransportError),
    #[error("timed out waiting for the language server response to {method}")]
    Timeout { method: String },
    #[error("the {method} request was cancelled by its caller")]
    Cancelled { method: String },
    #[error("language server request {method} failed ({code}): {message}")]
    Server {
        method: String,
        code: i32,
        message: String,
    },
    #[error("failed to encode the {method} request: {message}")]
    InvalidParams { method: String, message: String },
    #[error("request id overflow")]
    RequestIdOverflow,
}

/// Server-originated traffic observed while the client pumps messages.
/// Server-to-client requests are answered by the built-in minimal responder
/// before being surfaced here.
#[derive(Debug)]
pub enum ServerEvent {
    Notification { method: String, params: Value },
    ServerRequest { method: String, params: Value },
    Closed(String),
}

/// Transport liveness as observed by the client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Health {
    Alive,
    Closed(String),
}

/// Exponential restart delay with a cap. Providers wait `next_delay()` before
/// each restart attempt and `reset()` after a session starts successfully.
#[derive(Debug, Clone)]
pub struct RestartBackoff {
    base: Duration,
    max: Duration,
    attempt: u32,
}

impl RestartBackoff {
    pub fn new(base: Duration, max: Duration) -> Self {
        let base = if base.is_zero() {
            Duration::from_millis(1)
        } else {
            base
        };
        Self {
            base,
            max: max.max(base),
            attempt: 0,
        }
    }

    pub fn next_delay(&mut self) -> Duration {
        let shift = self.attempt.min(31);
        let delay = self
            .base
            .checked_mul(1_u32 << shift)
            .unwrap_or(self.max)
            .min(self.max);
        self.attempt = self.attempt.saturating_add(1);
        delay
    }

    pub fn reset(&mut self) {
        self.attempt = 0;
    }
}

/// Owned single-connection LSP client. One request is in flight at a time;
/// the owning provider worker drives every call from its single thread.
#[derive(Debug)]
pub struct Client {
    transport: Option<Transport>,
    config: ClientConfig,
    next_request_id: i32,
    initialized: bool,
    closed: Option<String>,
}

impl Client {
    /// Spawns the child process and its pump threads. The process is owned:
    /// dropping the client terminates it (with a process-group kill fallback
    /// on Unix), so a failed initialization can never leak a server.
    pub fn spawn(
        program: &OsStr,
        args: &[&OsStr],
        root: &Path,
        config: ClientConfig,
        log_target: &'static str,
    ) -> Result<Self, ClientError> {
        if config.startup_timeout.is_zero() || config.shutdown_timeout.is_zero() {
            return Err(ClientError::InvalidConfig);
        }
        let transport = Transport::spawn(program, args, root, &config.transport, log_target)?;
        Ok(Self {
            transport: Some(transport),
            config,
            next_request_id: 1,
            initialized: false,
            closed: None,
        })
    }

    pub fn health(&self) -> Health {
        match &self.closed {
            Some(reason) => Health::Closed(reason.clone()),
            None => Health::Alive,
        }
    }

    /// Bounded `initialize`/`initialized` handshake. Returns the raw
    /// `InitializeResult` payload for the provider to decode.
    pub fn initialize(
        &mut self,
        params: &impl Serialize,
        events: &mut dyn FnMut(ServerEvent),
    ) -> Result<Value, ClientError> {
        let deadline = Instant::now() + self.config.startup_timeout;
        let result = self.request("initialize", params, deadline, None, events)?;
        self.notify("initialized", &serde_json::json!({}), deadline)?;
        self.initialized = true;
        Ok(result)
    }

    /// Sends one request and waits for its response, interleaving server
    /// notifications and server-to-client requests into `events`. On timeout
    /// or caller cancellation the server is sent `$/cancelRequest`.
    pub fn request(
        &mut self,
        method: &str,
        params: &impl Serialize,
        deadline: Instant,
        cancel: Option<&dyn Fn() -> bool>,
        events: &mut dyn FnMut(ServerEvent),
    ) -> Result<Value, ClientError> {
        let id = RequestId::from(self.next_request_id);
        self.next_request_id = self
            .next_request_id
            .checked_add(1)
            .ok_or(ClientError::RequestIdOverflow)?;
        let params = serde_json::to_value(params).map_err(|error| ClientError::InvalidParams {
            method: method.to_owned(),
            message: error.to_string(),
        })?;
        self.send(
            &Message::Request(Request {
                id: id.clone(),
                method: method.to_owned(),
                params,
            }),
            deadline,
        )?;
        match self.wait_for_response(method, &id, deadline, cancel, events) {
            Ok(value) => Ok(value),
            Err(error @ (ClientError::Timeout { .. } | ClientError::Cancelled { .. })) => {
                let cancel_deadline = Instant::now() + self.config.shutdown_timeout;
                let _ = self.notify(
                    "$/cancelRequest",
                    &serde_json::json!({ "id": id }),
                    cancel_deadline,
                );
                Err(error)
            }
            Err(error) => Err(error),
        }
    }

    /// Sends one notification. Serialization and transport failures are
    /// returned; the server never answers notifications.
    pub fn notify(
        &mut self,
        method: &str,
        params: &impl Serialize,
        deadline: Instant,
    ) -> Result<(), ClientError> {
        let params = serde_json::to_value(params).map_err(|error| ClientError::InvalidParams {
            method: method.to_owned(),
            message: error.to_string(),
        })?;
        self.send(
            &Message::Notification(Notification {
                method: method.to_owned(),
                params,
            }),
            deadline,
        )
    }

    /// Pumps already-received server traffic without blocking. Providers call
    /// this between queries so progress notifications stay current.
    pub fn drain_events(&mut self, events: &mut dyn FnMut(ServerEvent)) {
        loop {
            let event = {
                let Some(transport) = self.transport.as_ref() else {
                    return;
                };
                transport.incoming().try_recv()
            };
            match event {
                Ok(TransportEvent::Message(message)) => self.dispatch(message, events),
                Ok(TransportEvent::Closed(reason)) => {
                    self.closed = Some(reason.clone());
                    events(ServerEvent::Closed(reason));
                    return;
                }
                Err(_) => return,
            }
        }
    }

    /// Cooperative `shutdown` + `exit`, then the transport-level kill
    /// fallback. Idempotent; bounded by `shutdown_timeout`.
    pub fn shutdown(&mut self) {
        let Some(mut transport) = self.transport.take() else {
            return;
        };
        if self.initialized && self.closed.is_none() {
            let deadline = Instant::now() + self.config.shutdown_timeout;
            let id = RequestId::from(self.next_request_id);
            self.next_request_id = self.next_request_id.saturating_add(1);
            let request = Message::Request(Request {
                id: id.clone(),
                method: "shutdown".to_owned(),
                params: Value::Null,
            });
            if transport.send(&request, deadline).is_ok() {
                while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
                    match transport.incoming().recv_timeout(remaining.min(IDLE_POLL)) {
                        Ok(TransportEvent::Message(Message::Response(response)))
                            if response.id == id =>
                        {
                            break;
                        }
                        Ok(TransportEvent::Message(_)) => continue,
                        Ok(TransportEvent::Closed(_)) => break,
                        Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                            if Instant::now() >= deadline {
                                break;
                            }
                        }
                        Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
                    }
                }
                let _ = transport.send(
                    &Message::Notification(Notification {
                        method: "exit".to_owned(),
                        params: Value::Null,
                    }),
                    deadline,
                );
            }
        }
        transport.terminate();
    }

    fn send(&mut self, message: &Message, deadline: Instant) -> Result<(), ClientError> {
        if let Some(reason) = &self.closed {
            return Err(TransportError::Closed(reason.clone()).into());
        }
        let transport = self
            .transport
            .as_mut()
            .ok_or_else(|| TransportError::Closed("client is shut down".to_owned()))?;
        let result = transport.send(message, deadline);
        if let Err(TransportError::Closed(reason)) = &result {
            self.closed = Some(reason.clone());
        }
        result.map_err(ClientError::Transport)
    }

    fn wait_for_response(
        &mut self,
        method: &str,
        id: &RequestId,
        deadline: Instant,
        cancel: Option<&dyn Fn() -> bool>,
        events: &mut dyn FnMut(ServerEvent),
    ) -> Result<Value, ClientError> {
        loop {
            if cancel.is_some_and(|cancel| cancel()) {
                return Err(ClientError::Cancelled {
                    method: method.to_owned(),
                });
            }
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .ok_or_else(|| ClientError::Timeout {
                    method: method.to_owned(),
                })?;
            let event = {
                let transport = self.transport.as_ref().ok_or_else(|| {
                    ClientError::Transport(TransportError::Closed("client is shut down".to_owned()))
                })?;
                match transport.incoming().recv_timeout(remaining.min(IDLE_POLL)) {
                    Ok(event) => event,
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
                    Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                        return Err(ClientError::Transport(TransportError::Closed(
                            "reader disconnected".to_owned(),
                        )));
                    }
                }
            };
            match event {
                TransportEvent::Message(Message::Response(response)) if &response.id == id => {
                    return response
                        .response_result
                        .map_err(|error| ClientError::Server {
                            method: method.to_owned(),
                            code: error.code,
                            message: error.message,
                        });
                }
                TransportEvent::Message(message) => self.dispatch(message, events),
                TransportEvent::Closed(reason) => {
                    self.closed = Some(reason.clone());
                    events(ServerEvent::Closed(reason.clone()));
                    return Err(TransportError::Closed(reason).into());
                }
            }
        }
    }

    fn dispatch(&mut self, message: Message, events: &mut dyn FnMut(ServerEvent)) {
        match message {
            Message::Notification(notification) => events(ServerEvent::Notification {
                method: notification.method,
                params: notification.params,
            }),
            Message::Request(request) => {
                let method = request.method.clone();
                let params = request.params.clone();
                self.respond_to_server(request);
                events(ServerEvent::ServerRequest { method, params });
            }
            Message::Response(_) => {}
        }
    }

    /// Minimal built-in responder for server-to-client requests, mirroring
    /// the rust-analyzer adapter: configuration sections resolve to null and
    /// progress/registration requests are acknowledged.
    fn respond_to_server(&mut self, request: Request) {
        let result = match request.method.as_str() {
            "workspace/configuration" => {
                let count = request
                    .params
                    .get("items")
                    .and_then(Value::as_array)
                    .map_or(0, Vec::len);
                Ok(Value::Array(vec![Value::Null; count]))
            }
            "window/workDoneProgress/create"
            | "client/registerCapability"
            | "client/unregisterCapability" => Ok(Value::Null),
            _ => Err(ResponseError {
                code: ErrorCode::MethodNotFound as i32,
                message: format!("unsupported client request: {}", request.method),
                data: None,
            }),
        };
        let deadline = Instant::now() + self.config.shutdown_timeout;
        let _ = self.send(
            &Message::Response(Response {
                id: request.id,
                response_result: result,
            }),
            deadline,
        );
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_to_its_cap_and_resets() {
        let mut backoff =
            RestartBackoff::new(Duration::from_millis(50), Duration::from_millis(120));
        assert_eq!(backoff.next_delay(), Duration::from_millis(50));
        assert_eq!(backoff.next_delay(), Duration::from_millis(100));
        assert_eq!(backoff.next_delay(), Duration::from_millis(120));
        assert_eq!(backoff.next_delay(), Duration::from_millis(120));
        backoff.reset();
        assert_eq!(backoff.next_delay(), Duration::from_millis(50));
    }

    #[test]
    fn backoff_clamps_a_zero_base() {
        let mut backoff = RestartBackoff::new(Duration::ZERO, Duration::from_millis(5));
        assert_eq!(backoff.next_delay(), Duration::from_millis(1));
    }
}
